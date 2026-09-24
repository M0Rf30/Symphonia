// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CELT decoder proper. Ported from libopus `celt/celt_decoder.c`
//! (`struct OpusCustomDecoder`, `celt_decoder_init`, `celt_decode_with_ec`,
//! `celt_decode_lost`, `celt_synthesis`, `tf_decode`, `deemphasis`, `prefilter_and_fold`).
//! Restricted to the float build (`FIXED_POINT` undefined) and `ENABLE_DEEP_PLC`/
//! `ENABLE_DRED`/`CUSTOM_MODES` excluded, matching the wave-1 scope.
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".
//!
//! This is the integration point wave 1's "CeltSynthesis" agent writes against the signatures
//! in [`crate::celt::bands`], [`crate::celt::rate`], [`crate::celt::quant_bands`] (owned by
//! "CeltBitstream") plus this module's own [`crate::celt::mdct`]/[`crate::celt::kiss_fft`]/
//! [`crate::celt::pitch`]/[`crate::celt::lpc`]/[`crate::celt::celt`] helpers.

use crate::celt::celt;
use crate::celt::lpc::{self, CELT_LPC_ORDER};
use crate::celt::mdct::MDCT_LOOKUP_960;
use crate::celt::modes::CeltMode;
use crate::celt::pitch;
use crate::celt::{bands, quant_bands, rate};
use crate::range::{RangeDecoder, BITRES};

/// C: `DECODE_BUFFER_SIZE`.
const DECODE_BUFFER_SIZE: usize = 2048;
/// C: `MAX_PERIOD` (`celt/modes.h`).
const MAX_PERIOD: usize = 1024;
/// C: `PLC_PITCH_LAG_MAX`.
const PLC_PITCH_LAG_MAX: i32 = 720;
/// C: `PLC_PITCH_LAG_MIN`.
const PLC_PITCH_LAG_MIN: i32 = 100;
/// C: `mode->preemph[0]` for the built-in 48 kHz mode (`static_modes_float.h`:
/// `{0.85000610f, 0.0000000f, 1.0000000f, 1.0000000f}`; only `preemph[0]` matters since
/// `preemph[1] == 0` disables the `CUSTOM_MODES`-only FIR-deemphasis branch, and `preemph[2]`/
/// `preemph[3]` are exactly `1` for this mode).
const PREEMPH_COEF0: f32 = 0.85000610;

/// C: `tapset_icdf[3]`.
const TAPSET_ICDF: [u8; 3] = [2, 1, 0];
/// C: `spread_icdf[4]`.
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
/// C: `trim_icdf[11]`.
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
/// C: `SPREAD_NORMAL` (`celt/bands.h`).
const SPREAD_NORMAL: i32 = 2;

const OPUS_BAD_ARG: i32 = -1;
const OPUS_INTERNAL_ERROR: i32 = -3;

/// C: `struct OpusCustomDecoder`, restricted to decoder-relevant fields (no `arch`/encoder
/// fields; `complexity`/`arch` are dropped entirely since this port has neither an encoder nor
/// SIMD kernels to select between).
pub struct CeltDecoder {
    mode: &'static CeltMode,
    overlap: i32,
    channels: i32,
    stream_channels: i32,
    downsample: i32,
    start: i32,
    end: i32,
    signalling: bool,
    disable_inv: bool,

    // Cleared on `celt_decoder_ctl(CELT_RESET_STATE)` / reset, C: everything from
    // `DECODER_RESET_START` (`rng`) onward.
    rng: u32,
    error: i32,
    last_pitch_index: i32,
    loss_duration: i32,
    skip_plc: bool,
    postfilter_period: i32,
    postfilter_period_old: i32,
    postfilter_gain: f32,
    postfilter_gain_old: f32,
    postfilter_tapset: i32,
    postfilter_tapset_old: i32,
    prefilter_and_fold: bool,
    preemph_mem: [f32; 2],

    /// C: `_decode_mem` (flattened `channels * (DECODE_BUFFER_SIZE + overlap)`, one contiguous
    /// per-channel history buffer whose tail `[DECODE_BUFFER_SIZE-N, DECODE_BUFFER_SIZE+overlap)`
    /// is `out_syn[c]` for the current frame).
    decode_mem: Vec<f32>,
    /// C: `lpc` (flattened `channels * CELT_LPC_ORDER`), PLC-only.
    lpc: Vec<f32>,
    /// C: `oldBandE` (flattened `2 * nbEBands`; always sized for 2 channels regardless of
    /// `stream_channels`, matching the C layout other code relies on for the `C==1` mono
    /// mirroring trick).
    old_e_bands: Vec<f32>,
    /// C: `oldLogE`.
    old_log_e: Vec<f32>,
    /// C: `oldLogE2`.
    old_log_e2: Vec<f32>,
    /// C: `backgroundLogE`.
    background_log_e: Vec<f32>,
    /// Preallocated scratch for [`Self::deemphasis`]'s `downsample > 1` path (C: `scratch`,
    /// `ALLOC(scratch, N, celt_sig)`); sized to the largest possible frame (`shortMdctSize <<
    /// maxLM`) so no per-packet heap allocation is needed on the decode hot path.
    deemphasis_scratch: Vec<f32>,
}

impl CeltDecoder {
    /// C: `celt_decoder_init` (`opus_custom_decoder_init` restricted to the built-in 48 kHz
    /// mode, matching how `opus_decoder.c` always calls it).
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let mode = &crate::celt::modes::MODE_48000_960;
        let channels = channels as i32;
        let nb_ebands = mode.nb_ebands as usize;
        let stride = DECODE_BUFFER_SIZE + mode.overlap as usize;
        let max_n = (mode.short_mdct_size << mode.max_lm) as usize;
        let downsample = celt::resampling_factor(sample_rate as i32).max(1);
        let mut d = CeltDecoder {
            mode,
            overlap: mode.overlap,
            channels,
            stream_channels: channels,
            downsample,
            start: 0,
            end: mode.effective_ebands,
            signalling: true,
            disable_inv: channels == 1,
            rng: 0,
            error: 0,
            last_pitch_index: 0,
            loss_duration: 0,
            skip_plc: false,
            postfilter_period: 0,
            postfilter_period_old: 0,
            postfilter_gain: 0.0,
            postfilter_gain_old: 0.0,
            postfilter_tapset: 0,
            postfilter_tapset_old: 0,
            prefilter_and_fold: false,
            preemph_mem: [0.0; 2],
            decode_mem: vec![0.0; channels as usize * stride],
            lpc: vec![0.0; channels as usize * CELT_LPC_ORDER],
            old_e_bands: vec![0.0; 2 * nb_ebands],
            old_log_e: vec![0.0; 2 * nb_ebands],
            old_log_e2: vec![0.0; 2 * nb_ebands],
            background_log_e: vec![0.0; 2 * nb_ebands],
            deemphasis_scratch: vec![0.0; max_n],
        };
        d.reset();
        d
    }

    fn decode_mem_stride(&self) -> usize {
        DECODE_BUFFER_SIZE + self.overlap as usize
    }

    /// C: `celt_decoder_ctl(CELT_RESET_STATE)`.
    pub fn reset(&mut self) {
        self.rng = 0;
        self.error = 0;
        self.last_pitch_index = 0;
        self.loss_duration = 0;
        self.postfilter_period = 0;
        self.postfilter_period_old = 0;
        self.postfilter_gain = 0.0;
        self.postfilter_gain_old = 0.0;
        self.postfilter_tapset = 0;
        self.postfilter_tapset_old = 0;
        self.prefilter_and_fold = false;
        self.preemph_mem = [0.0; 2];
        self.decode_mem.iter_mut().for_each(|v| *v = 0.0);
        self.lpc.iter_mut().for_each(|v| *v = 0.0);
        self.old_e_bands.iter_mut().for_each(|v| *v = 0.0);
        self.old_log_e.iter_mut().for_each(|v| *v = -28.0);
        self.old_log_e2.iter_mut().for_each(|v| *v = -28.0);
        self.background_log_e.iter_mut().for_each(|v| *v = 0.0);
        self.skip_plc = true;
    }

    /// C: `celt_decoder_ctl(CELT_SET_START_BAND(x))`. Hybrid mode uses `start = 17`.
    pub fn set_start_band(&mut self, start: i32) {
        self.start = start;
    }

    /// C: `celt_decoder_ctl(CELT_SET_END_BAND(x))`.
    pub fn set_end_band(&mut self, end: i32) {
        self.end = end;
    }

    /// C: `celt_decoder_ctl(CELT_SET_CHANNELS(x))` — the number of *coded* stream channels,
    /// which may differ from `self.channels` (the API-level output channel count).
    pub fn set_channels(&mut self, stream_channels: i32) {
        self.stream_channels = stream_channels;
    }

    /// C: `celt_decoder_ctl(CELT_SET_SIGNALLING(x))`. Opus always disables in-band signalling
    /// (`0`), since mode/bandwidth signalling is carried by the Opus TOC instead.
    pub fn set_signalling(&mut self, signalling: bool) {
        self.signalling = signalling;
    }

    /// C: `celt_decoder_ctl(OPUS_GET_AND_CLEAR_ERROR(&x))` (conceptually — libopus has no
    /// direct equivalent CTL for CELT; `st->error` is read after `celt_decode_with_ec` returns
    /// nonzero instead). Returns and clears the last decode error code, if any.
    pub fn get_and_clear_error(&mut self) -> i32 {
        let e = self.error;
        self.error = 0;
        e
    }

    /// C: `celt_decoder_ctl(CELT_GET_AND_CLEAR_ERROR(&x))`'s sibling `st->rng` accessor, used
    /// by `opus_decode_frame` to compute `st->rangeFinal` for Hybrid/CELT packets.
    pub fn final_range(&self) -> u32 {
        self.rng
    }

    /// C: `deemphasis`. `accum` is always `false` in the float build (`celt_assert(accum==0)`
    /// in `celt_decoder.c` — the accumulating branches are `FIXED_POINT`-only); kept as a
    /// parameter for API symmetry with the C signature/wave-2's call site.
    fn deemphasis(&mut self, out: &mut [f32], n: i32, cc: i32, accum: bool) {
        debug_assert!(!accum, "accum is FIXED_POINT-only in libopus; the float build never sets it");
        let n = n as usize;
        let cc = cc as usize;
        let downsample = self.downsample as usize;
        let nd = n / downsample;
        let stride = self.decode_mem_stride();
        for c in 0..cc {
            let mut m = self.preemph_mem[c];
            let base = c * stride + (DECODE_BUFFER_SIZE - n);
            if downsample > 1 {
                let scratch = &mut self.deemphasis_scratch[..n];
                for j in 0..n {
                    let tmp = self.decode_mem[base + j] + m;
                    m = PREEMPH_COEF0 * tmp;
                    scratch[j] = tmp;
                }
                for j in 0..nd {
                    out[c + j * cc] = scratch[j * downsample];
                }
            }
            else {
                for j in 0..n {
                    let tmp = self.decode_mem[base + j] + m;
                    m = PREEMPH_COEF0 * tmp;
                    out[c + j * cc] = tmp;
                }
            }
            self.preemph_mem[c] = m;
        }
    }

    /// C: `celt_synthesis`. Denormalises `x` (`c` interleaved-by-block channels of normalized
    /// MDCT shape, length `n` each) via [`bands::denormalise_bands`] and inverse-MDCTs the
    /// result into this decoder's persistent `decode_mem` (`out_syn`), handling the mono<->
    /// stereo up/down-mix cases.
    #[allow(clippy::too_many_arguments)]
    fn celt_synthesis(
        &mut self,
        x: &[f32],
        start: i32,
        eff_end: i32,
        c: i32,
        cc: i32,
        is_transient: bool,
        lm: i32,
        silence: bool,
    ) {
        let mode = self.mode;
        let overlap = mode.overlap;
        let nb_ebands = mode.nb_ebands as usize;
        let n = (mode.short_mdct_size << lm) as usize;
        let m = 1i32 << lm;
        let (b, nb, shift) = if is_transient {
            (m, mode.short_mdct_size, mode.max_lm)
        }
        else {
            (1, mode.short_mdct_size << lm, mode.max_lm - lm)
        };
        let stride = self.decode_mem_stride();
        let downsample = self.downsample;

        let mut freq = vec![0f32; n];
        let mut freq2 = vec![0f32; n];

        if cc == 2 && c == 1 {
            // Copying a mono stream to two channels.
            bands::denormalise_bands(mode, x, &mut freq, &self.old_e_bands, start, eff_end, m, downsample, silence);
            freq2.copy_from_slice(&freq);
            let (chan0, chan1) = self.decode_mem.split_at_mut(stride);
            let out0 = &mut chan0[DECODE_BUFFER_SIZE - n..];
            let out1 = &mut chan1[DECODE_BUFFER_SIZE - n..];
            for bidx in 0..b as usize {
                MDCT_LOOKUP_960.backward(
                    &freq2[bidx..],
                    &mut out0[nb as usize * bidx..],
                    mode.window,
                    overlap,
                    shift,
                    b,
                );
            }
            for bidx in 0..b as usize {
                MDCT_LOOKUP_960.backward(
                    &freq[bidx..],
                    &mut out1[nb as usize * bidx..],
                    mode.window,
                    overlap,
                    shift,
                    b,
                );
            }
        }
        else if cc == 1 && c == 2 {
            // Downmixing a stereo stream to mono.
            bands::denormalise_bands(mode, x, &mut freq, &self.old_e_bands, start, eff_end, m, downsample, silence);
            bands::denormalise_bands(
                mode,
                &x[n..],
                &mut freq2,
                &self.old_e_bands[nb_ebands..],
                start,
                eff_end,
                m,
                downsample,
                silence,
            );
            for i in 0..n {
                freq[i] = 0.5 * freq[i] + 0.5 * freq2[i];
            }
            let out0 = &mut self.decode_mem[DECODE_BUFFER_SIZE - n..stride];
            for bidx in 0..b as usize {
                MDCT_LOOKUP_960.backward(
                    &freq[bidx..],
                    &mut out0[nb as usize * bidx..],
                    mode.window,
                    overlap,
                    shift,
                    b,
                );
            }
        }
        else {
            // Normal case (mono or stereo, `C == CC`).
            for ch in 0..cc as usize {
                bands::denormalise_bands(
                    mode,
                    &x[ch * n..],
                    &mut freq,
                    &self.old_e_bands[ch * nb_ebands..],
                    start,
                    eff_end,
                    m,
                    downsample,
                    silence,
                );
                let base = ch * stride;
                let out = &mut self.decode_mem[base + DECODE_BUFFER_SIZE - n..base + stride];
                for bidx in 0..b as usize {
                    MDCT_LOOKUP_960.backward(
                        &freq[bidx..],
                        &mut out[nb as usize * bidx..],
                        mode.window,
                        overlap,
                        shift,
                        b,
                    );
                }
            }
        }
        // C: `SATURATE(out_syn[c][i], SIG_SAT)` is the identity in the float build (`#define
        // SATURATE(x,a) (x)`), so no saturation pass is needed here.
    }

    /// C: `tf_decode`. `total_bits` is `dec->storage*8` (C: `budget = dec->storage*8`) — the
    /// packet's total real-bit budget, i.e. `len*8`, which the caller already tracks locally
    /// (`range.rs` doesn't expose `storage` directly).
    fn tf_decode(
        start: i32,
        end: i32,
        is_transient: bool,
        tf_res: &mut [i32],
        lm: i32,
        rd: &mut RangeDecoder<'_>,
        total_bits: i32,
    ) {
        let budget = total_bits as i64;
        let mut tell = rd.tell() as i64;
        let mut logp: u32 = if is_transient { 2 } else { 4 };
        let tf_select_rsv = lm > 0 && tell + logp as i64 + 1 <= budget;
        let budget = budget - tf_select_rsv as i64;
        let mut tf_changed = false;
        let mut curr = false;
        for i in start..end {
            if tell + logp as i64 <= budget {
                curr ^= rd.dec_bit_logp(logp);
                tell = rd.tell() as i64;
                tf_changed |= curr;
            }
            tf_res[i as usize] = curr as i32;
            logp = if is_transient { 4 } else { 5 };
        }
        let mut tf_select = 0usize;
        if tf_select_rsv {
            let a = celt::TF_SELECT_TABLE[lm as usize][4 * is_transient as usize + tf_changed as usize];
            let b = celt::TF_SELECT_TABLE[lm as usize][4 * is_transient as usize + 2 + tf_changed as usize];
            if a != b {
                tf_select = rd.dec_bit_logp(1) as usize;
            }
        }
        for i in start..end {
            let idx = 4 * is_transient as usize + 2 * tf_select + tf_res[i as usize] as usize;
            tf_res[i as usize] = celt::TF_SELECT_TABLE[lm as usize][idx] as i32;
        }
    }

    /// C: `prefilter_and_fold`. Applies the (negated) pre-filter to the MDCT overlap tail of a
    /// lost frame and simulates TDAC on the concealed audio so the next real frame's overlap-add
    /// blends smoothly with it.
    fn prefilter_and_fold(&mut self, n: i32) {
        let mode = self.mode;
        let overlap = self.overlap as usize;
        let cc = self.channels as usize;
        let stride = self.decode_mem_stride();
        let n = n as usize;
        let mut etmp = vec![0f32; overlap];
        for c in 0..cc {
            let base = c * stride + DECODE_BUFFER_SIZE - n;
            celt::comb_filter_copy(
                &mut etmp,
                &self.decode_mem,
                base,
                self.postfilter_period,
                overlap as i32,
                -self.postfilter_gain,
                self.postfilter_tapset,
            );
            // C's `comb_filter` full form also mixes in `T0`/`g0`/`tapset0` via the crossfade
            // loop when `window != NULL`; `prefilter_and_fold` always calls with `window=NULL,
            // overlap=0`, so [`celt::comb_filter_copy`] (which drops the dead `T0`/`g0`/
            // `tapset0` parameters) is the exact match — see its doc comment.
            for i in 0..overlap / 2 {
                self.decode_mem[base + i] =
                    mode.window[i] * etmp[overlap - 1 - i] + mode.window[overlap - i - 1] * etmp[i];
            }
        }
    }

    /// C: `celt_plc_pitch_search`.
    fn celt_plc_pitch_search(&self) -> i32 {
        let cc = self.channels as usize;
        let stride = self.decode_mem_stride();
        let bufs: Vec<&[f32]> =
            (0..cc).map(|c| &self.decode_mem[c * stride..c * stride + DECODE_BUFFER_SIZE]).collect();
        let mut lp_pitch_buf = vec![0f32; DECODE_BUFFER_SIZE / 2];
        pitch::pitch_downsample(&bufs, &mut lp_pitch_buf, DECODE_BUFFER_SIZE as i32, cc as i32);
        let pitch_index = pitch::pitch_search(
            &lp_pitch_buf[(PLC_PITCH_LAG_MAX / 2) as usize..],
            &lp_pitch_buf,
            DECODE_BUFFER_SIZE as i32 - PLC_PITCH_LAG_MAX,
            PLC_PITCH_LAG_MAX - PLC_PITCH_LAG_MIN,
        );
        PLC_PITCH_LAG_MAX - pitch_index
    }

    /// C: `celt_decode_lost`. Packet-loss concealment: noise-based (for hybrid/long losses/
    /// silence-skip) or pitch-based (extrapolating a decaying periodic waveform).
    fn celt_decode_lost(&mut self, n: i32, lm: i32) {
        let mode = self.mode;
        let nb_ebands = mode.nb_ebands as usize;
        let overlap = self.overlap;
        let cc = self.channels;
        let start = self.start;
        let stride = self.decode_mem_stride();
        let loss_duration = self.loss_duration;

        let noise_based = loss_duration >= 40 || start != 0 || self.skip_plc;
        if noise_based {
            let end = self.end;
            let eff_end = start.max(end.min(mode.effective_ebands));
            let mut x = vec![0f32; (cc * n) as usize];
            for c in 0..cc as usize {
                let base = c * stride;
                self.decode_mem.copy_within(base + n as usize..base + stride, base);
            }
            let decay = if loss_duration == 0 { 1.5f32 } else { 0.5f32 };
            for c in 0..cc as usize {
                for i in (start as usize)..(end as usize) {
                    let idx = c * nb_ebands + i;
                    self.old_e_bands[idx] = self.background_log_e[idx].max(self.old_e_bands[idx] - decay);
                }
            }
            let mut seed = self.rng;
            let m = 1i32 << lm;
            for c in 0..cc as usize {
                for i in (start as usize)..(eff_end as usize) {
                    let boffs = n as usize * c + (mode.e_bands[i] as i32 * m) as usize;
                    let blen = ((mode.e_bands[i + 1] - mode.e_bands[i]) as i32 * m) as usize;
                    for j in 0..blen {
                        seed = celt_lcg_rand(seed);
                        x[boffs + j] = ((seed as i32) >> 20) as f32;
                    }
                    crate::celt::vq::renormalise_vector(&mut x[boffs..boffs + blen], blen as i32, 1.0);
                }
            }
            self.rng = seed;

            self.celt_synthesis(&x, start, eff_end, cc, cc, false, lm, false);
            self.prefilter_and_fold = false;
            self.skip_plc = true;
        }
        else {
            let exc_length;
            let mut fade = 1.0f32;
            let pitch_index;
            if loss_duration == 0 {
                pitch_index = self.celt_plc_pitch_search();
                self.last_pitch_index = pitch_index;
            }
            else {
                pitch_index = self.last_pitch_index;
                fade = 0.8;
            }
            exc_length = (2 * pitch_index).min(MAX_PERIOD as i32) as usize;

            let window = self.mode.window;
            for c in 0..cc as usize {
                let mut exc_full = vec![0f32; MAX_PERIOD + CELT_LPC_ORDER];
                let base = c * stride;
                for i in 0..MAX_PERIOD + CELT_LPC_ORDER {
                    exc_full[i] = self.decode_mem[base + DECODE_BUFFER_SIZE - MAX_PERIOD - CELT_LPC_ORDER + i];
                }

                if loss_duration == 0 {
                    let mut ac = [0f32; CELT_LPC_ORDER + 1];
                    lpc::celt_autocorr(
                        &exc_full[CELT_LPC_ORDER..],
                        &mut ac,
                        Some(window),
                        overlap,
                        CELT_LPC_ORDER as i32,
                        MAX_PERIOD as i32,
                    );
                    ac[0] *= 1.0001;
                    for i in 1..=CELT_LPC_ORDER {
                        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
                    }
                    lpc::celt_lpc(&mut self.lpc[c * CELT_LPC_ORDER..], &ac, CELT_LPC_ORDER as i32);
                }

                {
                    // C: `celt_fir(exc+MAX_PERIOD-exc_length, lpc, fir_tmp, exc_length,
                    // CELT_LPC_ORDER, arch)` where C's `exc` pointer is `_exc+CELT_LPC_ORDER`, so
                    // the FIR needs `CELT_LPC_ORDER` samples of history *before* that — which, for
                    // large `exc_length` (up to `MAX_PERIOD`, i.e. `x_pos == 0` relative to the
                    // sliced view), only exist in the un-sliced `exc_full` prefix. Pass the
                    // un-sliced buffer with the correspondingly shifted `x_pos` instead of slicing
                    // away that history.
                    let mut fir_tmp = vec![0f32; exc_length];
                    let lpc_c = self.lpc[c * CELT_LPC_ORDER..c * CELT_LPC_ORDER + CELT_LPC_ORDER].to_vec();
                    lpc::celt_fir(
                        &exc_full,
                        CELT_LPC_ORDER + MAX_PERIOD - exc_length,
                        &lpc_c,
                        &mut fir_tmp,
                        exc_length as i32,
                        CELT_LPC_ORDER as i32,
                    );
                    exc_full[CELT_LPC_ORDER + MAX_PERIOD - exc_length..CELT_LPC_ORDER + MAX_PERIOD]
                        .copy_from_slice(&fir_tmp);
                }

                let exc = &mut exc_full[CELT_LPC_ORDER..];

                let decay = {
                    let decay_length = exc_length / 2;
                    let mut e1 = 1f32;
                    let mut e2 = 1f32;
                    for i in 0..decay_length {
                        let e = exc[MAX_PERIOD - decay_length + i];
                        e1 += e * e;
                        let e = exc[MAX_PERIOD - 2 * decay_length + i];
                        e2 += e * e;
                    }
                    let e1 = e1.min(e2);
                    (0.5 * e1 / e2).sqrt()
                };

                // C: `OPUS_MOVE(buf, buf+N, DECODE_BUFFER_SIZE-N)` — left-shift the decoder
                // memory by `N` to make room for the new (concealed) frame at the tail.
                self.decode_mem.copy_within(base + n as usize..base + DECODE_BUFFER_SIZE, base);

                let extrapolation_offset = MAX_PERIOD as i32 - pitch_index;
                let extrapolation_len = n as i32 + overlap;
                let mut attenuation = fade * decay;
                let mut s1 = 0f32;
                let mut j = 0i32;
                for i in 0..extrapolation_len {
                    if j >= pitch_index {
                        j -= pitch_index;
                        attenuation *= decay;
                    }
                    let val = attenuation * exc[(extrapolation_offset + j) as usize];
                    self.decode_mem[base + DECODE_BUFFER_SIZE - n as usize + i as usize] = val;
                    let tmp = self.decode_mem
                        [base + DECODE_BUFFER_SIZE - MAX_PERIOD - n as usize + (extrapolation_offset + j) as usize];
                    s1 += tmp * tmp * (1.0 / 1024.0);
                    j += 1;
                }

                {
                    let mut lpc_mem = [0f32; CELT_LPC_ORDER];
                    for i in 0..CELT_LPC_ORDER {
                        lpc_mem[i] = self.decode_mem[base + DECODE_BUFFER_SIZE - n as usize - 1 - i];
                    }
                    let start_idx = base + DECODE_BUFFER_SIZE - n as usize;
                    let seg: Vec<f32> = self.decode_mem[start_idx..start_idx + extrapolation_len as usize].to_vec();
                    let mut y = vec![0f32; extrapolation_len as usize];
                    let lpc_c = self.lpc[c * CELT_LPC_ORDER..c * CELT_LPC_ORDER + CELT_LPC_ORDER].to_vec();
                    lpc::celt_iir(&seg, &lpc_c, &mut y, extrapolation_len, CELT_LPC_ORDER as i32, &mut lpc_mem);
                    self.decode_mem[start_idx..start_idx + extrapolation_len as usize].copy_from_slice(&y);
                }

                {
                    let start_idx = base + DECODE_BUFFER_SIZE - n as usize;
                    let mut s2 = 0f32;
                    for i in 0..extrapolation_len as usize {
                        let tmp = self.decode_mem[start_idx + i];
                        s2 += tmp * tmp * (1.0 / 1024.0);
                    }
                    if !(s1 > 0.2 * s2) {
                        for i in 0..extrapolation_len as usize {
                            self.decode_mem[start_idx + i] = 0.0;
                        }
                    }
                    else if s1 < s2 {
                        let ratio = (0.5 * s1 + 1.0) / (s2 + 1.0);
                        let ratio = ratio.sqrt();
                        for i in 0..overlap as usize {
                            let tmp_g = 1.0 - window[i] * (1.0 - ratio);
                            self.decode_mem[start_idx + i] *= tmp_g;
                        }
                        for i in overlap as usize..extrapolation_len as usize {
                            self.decode_mem[start_idx + i] *= ratio;
                        }
                    }
                }
            }
            self.prefilter_and_fold = true;
        }
        self.loss_duration = 10000.min(loss_duration + (1 << lm));
    }

    /// C: `celt_decode_with_ec`. `data == None` requests PLC for `frame_size` samples (mirrors
    /// the C `data == NULL` convention, matching `crate::decoder::OpusDecoder::decode`).
    /// `rd` is `None` exactly when `data` is `None` (PLC never touches the range coder); when
    /// `Some`, it is the *same* range-coder instance SILK decoded from, for Hybrid packets
    /// (see `crate::decoder` module docs on hybrid range-coder sharing).
    pub fn decode_with_ec<'a>(
        &mut self,
        data: Option<&'a [u8]>,
        out: &mut [f32],
        frame_size: i32,
        rd: Option<&mut RangeDecoder<'a>>,
        accum: bool,
    ) -> Result<usize, i32> {
        let cc = self.channels;
        let mode = self.mode;
        let nb_ebands = mode.nb_ebands as usize;
        let overlap = self.overlap;
        let start = self.start;
        let end = self.end;
        let frame_size = frame_size * self.downsample;

        let mut lm = 0i32;
        while lm <= mode.max_lm && (mode.short_mdct_size << lm) != frame_size {
            lm += 1;
        }
        if lm > mode.max_lm {
            return Err(OPUS_BAD_ARG);
        }
        let m = 1i32 << lm;
        let n = m * mode.short_mdct_size;

        let eff_end = end.min(mode.effective_ebands);

        if data.map_or(true, |d| d.len() <= 1) {
            self.celt_decode_lost(n, lm);
            self.deemphasis(out, n, cc, accum);
            return Ok((frame_size / self.downsample) as usize);
        }
        let data = data.unwrap();
        let len = data.len() as i32;
        if len > 1275 {
            return Err(OPUS_BAD_ARG);
        }

        if self.loss_duration == 0 {
            self.skip_plc = false;
        }

        let mut owned_rd;
        let rd: &mut RangeDecoder = match rd {
            Some(r) => r,
            None => {
                owned_rd = RangeDecoder::new(data);
                &mut owned_rd
            }
        };

        let c = self.stream_channels;
        if c == 1 {
            for i in 0..nb_ebands {
                self.old_e_bands[i] = self.old_e_bands[i].max(self.old_e_bands[nb_ebands + i]);
            }
        }

        let mut total_bits = len * 8;
        let mut tell = rd.tell();

        let silence = if tell >= total_bits { true } else if tell == 1 { rd.dec_bit_logp(15) } else { false };
        if silence {
            // NOTE: C additionally does `dec->nbits_total += tell-ec_tell(dec)` here so that
            // *every* later `ec_tell()`/`ec_tell_frac()` call (including inside
            // `unquant_coarse_energy`/`clt_compute_allocation`/etc., owned by "CeltBitstream")
            // also observes the packet as fully consumed. `range.rs` (wave 0, not ours to
            // modify) has no accessor for this; `tell`/`total_bits` are fixed up locally for
            // this function's own budget checks below, but the shared `rd` doesn't reflect it.
            // In practice `silence` requires an exact-zero-sample encoder decision (vanishingly
            // rare outside synthetic all-zero test input), so this is a known, narrow gap rather
            // than a general accuracy issue; revisit with a `RangeDecoder` coordination request
            // if a real vector's final range diverges on a silent packet.
            tell = len * 8;
        }

        let mut postfilter_gain = 0f32;
        let mut postfilter_pitch = 0i32;
        let mut postfilter_tapset = 0i32;
        if start == 0 && tell + 16 <= total_bits {
            if rd.dec_bit_logp(1) {
                let octave = rd.dec_uint(6);
                postfilter_pitch = (16 << octave) + rd.dec_bits(4 + octave) as i32 - 1;
                let qg = rd.dec_bits(3);
                if rd.tell() + 2 <= total_bits {
                    postfilter_tapset = rd.dec_icdf(&TAPSET_ICDF, 2);
                }
                postfilter_gain = 0.09375 * (qg as f32 + 1.0);
            }
            tell = rd.tell();
        }

        let is_transient = if lm > 0 && tell + 3 <= total_bits {
            let t = rd.dec_bit_logp(3);
            tell = rd.tell();
            t
        }
        else {
            false
        };

        let intra_ener = tell + 3 <= total_bits && rd.dec_bit_logp(3);
        if !intra_ener && self.loss_duration != 0 {
            for c_idx in 0..2usize {
                let safety = if lm == 0 {
                    1.5f32
                }
                else if lm == 1 {
                    0.5f32
                }
                else {
                    0.0f32
                };
                let missing = ((self.loss_duration >> lm).min(10)) as f32;
                for i in (start as usize)..(end as usize) {
                    let idx = c_idx * nb_ebands + i;
                    let e0 = self.old_e_bands[idx];
                    let e1 = self.old_log_e[idx];
                    let e2 = self.old_log_e2[idx];
                    if e0 < e1.max(e2) {
                        let slope = (e1 - e0).max(0.5 * (e2 - e0));
                        let e0 = e0 - 0f32.max((1.0 + missing) * slope);
                        self.old_e_bands[idx] = e0.max(-20.0);
                    }
                    else {
                        self.old_e_bands[idx] = e0.min(e1).min(e2);
                    }
                    self.old_e_bands[idx] -= safety;
                }
            }
        }

        quant_bands::unquant_coarse_energy(mode, start, end, &mut self.old_e_bands, intra_ener, rd, c, lm);

        let mut tf_res = vec![0i32; nb_ebands];
        Self::tf_decode(start, end, is_transient, &mut tf_res, lm, rd, total_bits);

        tell = rd.tell();
        let spread_decision = if tell + 4 <= total_bits { rd.dec_icdf(&SPREAD_ICDF, 5) } else { SPREAD_NORMAL };

        let mut cap = vec![0i32; nb_ebands];
        celt::init_caps(mode, &mut cap, lm, c);

        let mut offsets = vec![0i32; nb_ebands];
        let mut dynalloc_logp = 6i32;
        total_bits <<= BITRES;
        let mut tell_frac = rd.tell_frac() as i32;
        for i in (start as usize)..(end as usize) {
            let band_w = (mode.e_bands[i + 1] - mode.e_bands[i]) as i32;
            let width = c * (band_w << lm);
            let quanta = (width << BITRES).min((6 << BITRES).max(width));
            let mut dynalloc_loop_logp = dynalloc_logp;
            let mut boost = 0i32;
            while tell_frac + (dynalloc_loop_logp << BITRES) < total_bits && boost < cap[i] {
                let flag = rd.dec_bit_logp(dynalloc_loop_logp as u32);
                tell_frac = rd.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_bits -= quanta;
                dynalloc_loop_logp = 1;
            }
            offsets[i] = boost;
            if boost > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
        }

        let alloc_trim = if tell_frac + (6 << BITRES) <= total_bits { rd.dec_icdf(&TRIM_ICDF, 7) } else { 5 };

        let mut bits = ((len * 8) << BITRES) - rd.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if is_transient && lm >= 2 && bits >= ((lm + 2) << BITRES) { 1 << BITRES } else { 0 };
        bits -= anti_collapse_rsv;

        let alloc = rate::clt_compute_allocation(mode, start, end, &offsets, &cap, alloc_trim, bits, lm, c, rd);

        quant_bands::unquant_fine_energy(mode, start, end, &mut self.old_e_bands, &alloc.fine_energy_bits, rd, c);

        let stride = self.decode_mem_stride();
        for ch in 0..cc as usize {
            let base = ch * stride;
            self.decode_mem.copy_within(base + n as usize..base + stride, base);
        }

        let mut collapse_masks = vec![0u8; (c * mode.nb_ebands) as usize];
        let mut x = vec![0f32; (c * n) as usize];
        {
            let (x0, y) = if c == 2 {
                let (a, b) = x.split_at_mut(n as usize);
                (a, Some(b))
            }
            else {
                (&mut x[..], None)
            };
            bands::quant_all_bands(
                mode,
                start,
                end,
                x0,
                y,
                &mut collapse_masks,
                &alloc.pulses,
                is_transient,
                spread_decision,
                alloc.dual_stereo,
                alloc.intensity,
                &tf_res,
                len * (8 << BITRES) - anti_collapse_rsv,
                alloc.balance,
                rd,
                lm,
                alloc.coded_bands,
                &mut self.rng,
                self.disable_inv,
            );
        }

        let anti_collapse_on = anti_collapse_rsv > 0 && rd.dec_bits(1) != 0;

        quant_bands::unquant_energy_finalise(
            mode,
            start,
            end,
            &mut self.old_e_bands,
            &alloc.fine_energy_bits,
            &alloc.fine_priority,
            len * 8 - rd.tell(),
            rd,
            c,
        );

        if anti_collapse_on {
            bands::anti_collapse(
                mode,
                &mut x,
                &collapse_masks,
                lm,
                c,
                n,
                start,
                end,
                &self.old_e_bands,
                &self.old_log_e,
                &self.old_log_e2,
                &alloc.pulses,
                self.rng,
            );
        }

        if silence {
            for v in self.old_e_bands[..(c * mode.nb_ebands) as usize].iter_mut() {
                *v = -28.0;
            }
        }

        if self.prefilter_and_fold {
            self.prefilter_and_fold(n);
        }

        self.celt_synthesis(&x, start, eff_end, c, cc, is_transient, lm, silence);

        for ch in 0..cc as usize {
            self.postfilter_period = self.postfilter_period.max(celt::COMBFILTER_MINPERIOD);
            self.postfilter_period_old = self.postfilter_period_old.max(celt::COMBFILTER_MINPERIOD);
            let base = ch * stride + (DECODE_BUFFER_SIZE - n as usize);
            celt::comb_filter_inplace(
                &mut self.decode_mem,
                base,
                self.postfilter_period_old,
                self.postfilter_period,
                mode.short_mdct_size,
                self.postfilter_gain_old,
                self.postfilter_gain,
                self.postfilter_tapset_old,
                self.postfilter_tapset,
                mode.window,
                overlap,
            );
            if lm != 0 {
                celt::comb_filter_inplace(
                    &mut self.decode_mem,
                    base + mode.short_mdct_size as usize,
                    self.postfilter_period,
                    postfilter_pitch,
                    n - mode.short_mdct_size,
                    self.postfilter_gain,
                    postfilter_gain,
                    self.postfilter_tapset,
                    postfilter_tapset,
                    mode.window,
                    overlap,
                );
            }
        }
        self.postfilter_period_old = self.postfilter_period;
        self.postfilter_gain_old = self.postfilter_gain;
        self.postfilter_tapset_old = self.postfilter_tapset;
        self.postfilter_period = postfilter_pitch;
        self.postfilter_gain = postfilter_gain;
        self.postfilter_tapset = postfilter_tapset;
        if lm != 0 {
            self.postfilter_period_old = self.postfilter_period;
            self.postfilter_gain_old = self.postfilter_gain;
            self.postfilter_tapset_old = self.postfilter_tapset;
        }

        if c == 1 {
            for i in 0..nb_ebands {
                self.old_e_bands[nb_ebands + i] = self.old_e_bands[i];
            }
        }

        if !is_transient {
            self.old_log_e2.copy_from_slice(&self.old_log_e);
            self.old_log_e.copy_from_slice(&self.old_e_bands);
        }
        else {
            for i in 0..2 * nb_ebands {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_e_bands[i]);
            }
        }

        let max_background_increase = (160.min(self.loss_duration + m)) as f32 * 0.001;
        for i in 0..2 * nb_ebands {
            self.background_log_e[i] = (self.background_log_e[i] + max_background_increase).min(self.old_e_bands[i]);
        }

        for ch in 0..2usize {
            for i in 0..(start as usize) {
                self.old_e_bands[ch * nb_ebands + i] = 0.0;
                self.old_log_e[ch * nb_ebands + i] = -28.0;
                self.old_log_e2[ch * nb_ebands + i] = -28.0;
            }
            for i in (end as usize)..nb_ebands {
                self.old_e_bands[ch * nb_ebands + i] = 0.0;
                self.old_log_e[ch * nb_ebands + i] = -28.0;
                self.old_log_e2[ch * nb_ebands + i] = -28.0;
            }
        }
        self.rng = rd.range();

        self.deemphasis(out, n, cc, accum);
        self.loss_duration = 0;
        self.prefilter_and_fold = false;

        if rd.tell() > 8 * len {
            return Err(OPUS_INTERNAL_ERROR);
        }
        if rd.error() {
            self.error = 1;
        }
        Ok((frame_size / self.downsample) as usize)
    }
}

/// C: `celt_lcg_rand`.
fn celt_lcg_rand(seed: u32) -> u32 {
    1664525u32.wrapping_mul(seed).wrapping_add(1013904223)
}
