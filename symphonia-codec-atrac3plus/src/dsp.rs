// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Signal reconstruction: inverse quantization, power compensation, IMDCT, gain compensation,
//! sine wave synthesis and the inverse polyphase quadrature filter bank.
//!
//! This follows FFmpeg's `libavcodec/atrac3plusdsp.c`, `libavcodec/atrac.c` and the
//! reconstruction part of `libavcodec/atrac3plusdec.c`.

use std::f64::consts::PI;
use std::sync::LazyLock;

use symphonia_core::dsp::mdct::Imdct;

use crate::tables::*;
use crate::unit::*;

const TWOPI: f64 = 2.0 * PI;
/// ATRAC gain compensation: exponent offset of the level table, and log2 of the location scale.
const ID2EXP_OFFSET: i32 = 6;
const LOC_SCALE: u32 = 2;
const LOC_SIZE: usize = 1 << LOC_SCALE;

/// Maps a subband number to its power compensation group.
const SUBBAND_TO_POWGRP: [u8; SUBBANDS] = [0, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4];
/// Maps a subband number to its first quantization unit.
const SUBBAND_TO_QU: [u8; SUBBANDS + 1] =
    [0, 8, 12, 16, 18, 20, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32];
/// Number of taps in the inverse PQF filter, in units of 16 samples (half of the 384 taps).
const PQF_FIR_LEN: usize = 12;

/// Lookup table for the cyclic buffers of the inverse PQF (FFmpeg's `mod23_lut`).
const MOD23_LUT: [usize; 26] =
    [23, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 0];

/// Constant tables computed at start-up (FFmpeg's `ff_atrac3p_init_dsp_static`).
struct Tables {
    /// Sine wave table.
    sine: [f32; 2048],
    /// Hann window.
    hann: [f32; 256],
    /// Dequantized amplitude scale factors.
    amp_sf: [f32; 64],
    /// Sine windows of 128 and 64 samples.
    sine128: [f32; 128],
    sine64: [f32; 64],
    /// Gain levels, `2^(6 - i)`.
    gain_tab1: [f32; 16],
    /// Gain interpolation steps, `2^(-i / loc_size)` for `i = -15..=15`.
    gain_tab2: [f32; 31],
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let mut t = Tables {
        sine: [0.0; 2048],
        hann: [0.0; 256],
        amp_sf: [0.0; 64],
        sine128: [0.0; 128],
        sine64: [0.0; 64],
        gain_tab1: [0.0; 16],
        gain_tab2: [0.0; 31],
    };

    for (i, v) in t.sine.iter_mut().enumerate() {
        *v = (TWOPI * i as f64 / 2048.0).sin() as f32;
    }
    for (i, v) in t.hann.iter_mut().enumerate() {
        *v = ((1.0 - (TWOPI * f64::from(i as u32) / 256.0).cos()) * 0.5) as f32;
    }
    for (i, v) in t.amp_sf.iter_mut().enumerate() {
        *v = ((i as f32 - 3.0) / 4.0).exp2();
    }
    for (i, v) in t.sine128.iter_mut().enumerate() {
        *v = ((i as f64 + 0.5) / 256.0 * PI).sin() as f32;
    }
    for (i, v) in t.sine64.iter_mut().enumerate() {
        *v = ((i as f64 + 0.5) / 128.0 * PI).sin() as f32;
    }
    for (i, v) in t.gain_tab1.iter_mut().enumerate() {
        *v = 2.0f32.powf((ID2EXP_OFFSET - i as i32) as f32);
    }
    for (i, v) in t.gain_tab2.iter_mut().enumerate() {
        *v = 2.0f32.powf(-1.0 / LOC_SIZE as f32 * (i as i32 - 15) as f32);
    }
    t
});

/// Per-decoder synthesis state and scratch buffers.
pub(crate) struct Synth {
    tables: &'static Tables,
    imdct: Imdct,
    ipqf_imdct: Imdct,
    /// Dequantized spectrum per channel.
    spectrum: Box<[[f32; FRAME_SAMPLES]; 2]>,
    /// Subband signals after gain compensation, per channel.
    time_buf: Box<[f32; FRAME_SAMPLES]>,
    /// Output PCM per channel.
    pub(crate) output: Box<[[f32; FRAME_SAMPLES]; 2]>,
    imdct_in: [f32; SUBBAND_SAMPLES],
    imdct_out: [f32; 2 * SUBBAND_SAMPLES],
}

impl Synth {
    pub(crate) fn new() -> Self {
        Synth {
            tables: &TABLES,
            // FFmpeg: `av_tx_init(MDCT, inverse, 128, scale = -1.0, AV_TX_FULL_IMDCT)`.
            imdct: Imdct::new_scaled(SUBBAND_SAMPLES, -1.0),
            // FFmpeg: `av_tx_init(MDCT, inverse, 16, scale = 32.0 / 32768.0)`; the half-length
            // output of that transform is the middle of the full 32-sample IMDCT output.
            ipqf_imdct: Imdct::new_scaled(SUBBANDS, 32.0 / 32768.0),
            spectrum: Box::new([[0.0; FRAME_SAMPLES]; 2]),
            time_buf: Box::new([0.0; FRAME_SAMPLES]),
            output: Box::new([[0.0; FRAME_SAMPLES]; 2]),
            imdct_in: [0.0; SUBBAND_SAMPLES],
            imdct_out: [0.0; 2 * SUBBAND_SAMPLES],
        }
    }

    /// Reconstruct the PCM of a decoded channel unit into `self.output[..num_channels]`.
    pub(crate) fn synthesize(&mut self, unit: &mut ChanUnit, num_channels: usize) {
        self.decode_residual_spectrum(unit, num_channels);
        self.reconstruct_frame(unit, num_channels);
    }

    /// Inverse quantization, power compensation and joint stereo processing.
    fn decode_residual_spectrum(&mut self, unit: &ChanUnit, num_channels: usize) {
        for out in self.spectrum[..num_channels].iter_mut() {
            out.fill(0.0);
        }

        if unit.mute_flag {
            return;
        }

        // Calculate the RNG table index for each subband.
        let mut sb_rng_index = [0usize; SUBBANDS];
        let mut rng_index = 0i32;
        for qu in 0..unit.used_quant_units {
            rng_index += unit.channels[0].qu_sf_idx[qu] + unit.channels[1].qu_sf_idx[qu];
        }
        for rng in sb_rng_index[..unit.num_coded_subbands].iter_mut() {
            *rng = (rng_index & 0x3fc) as usize;
            rng_index += 128;
        }

        for ch in 0..num_channels {
            let chan = &unit.channels[ch];

            // Inverse quantization.
            for qu in 0..unit.used_quant_units {
                let start = usize::from(QU_TO_SPEC_POS[qu]);
                let end = usize::from(QU_TO_SPEC_POS[qu + 1]);

                let wordlen = chan.qu_wordlen[qu];
                if wordlen > 0 {
                    let q = SF_TAB[chan.qu_sf_idx[qu] as usize] * MANT_TAB[wordlen as usize];
                    for (d, &s) in
                        self.spectrum[ch][start..end].iter_mut().zip(&chan.spectrum[start..end])
                    {
                        *d = f32::from(s) * q;
                    }
                }
            }

            for sb in 0..unit.num_coded_subbands {
                self.power_compensation(unit, ch, sb_rng_index[sb], sb);
            }
        }

        if unit.unit_type == CH_UNIT_STEREO {
            let [left, right] = &mut *self.spectrum;
            for sb in 0..unit.num_coded_subbands {
                let range = sb * SUBBAND_SAMPLES..(sb + 1) * SUBBAND_SAMPLES;
                if unit.swap_channels[sb] != 0 {
                    left[range.clone()].swap_with_slice(&mut right[range.clone()]);
                }

                // Flip the sign of the coefficients if requested.
                if unit.negate_coeffs[sb] != 0 {
                    for v in right[range].iter_mut() {
                        *v = -*v;
                    }
                }
            }
        }
    }

    /// Add noise to the spectrum of a subband to compensate for the power of dropped
    /// coefficients (FFmpeg's `ff_atrac3p_power_compensation`).
    fn power_compensation(&mut self, unit: &ChanUnit, ch: usize, rng_index: usize, sb: usize) {
        let swap_ch = usize::from(unit.unit_type == CH_UNIT_STEREO && unit.swap_channels[sb] != 0);
        let src = &unit.channels[ch ^ swap_ch];
        let powgrp = usize::from(SUBBAND_TO_POWGRP[sb]);

        if src.power_levs[powgrp] == POWER_COMP_OFF {
            return;
        }

        let cur = unit.cur;
        let g1 = &src.gain_data[cur][sb];
        let g2 = &src.gain_data[cur ^ 1][sb];

        let gain_lev = if g1.num_points > 0 { 6 - g1.lev_code[0] } else { 0 };

        let mut gcv = 0;
        for &lev in &g2.lev_code[..g2.num_points] {
            gcv = gcv.max(gain_lev - (lev - 6));
        }
        for &lev in &g1.lev_code[..g1.num_points] {
            gcv = gcv.max(6 - lev);
        }

        let grp_lev = PWC_LEVS[usize::from(src.power_levs[powgrp])] / (1u32 << gcv) as f32;

        let first_qu = usize::from(SUBBAND_TO_QU[sb]) + if sb == 0 { 2 } else { 0 };
        let chan = &unit.channels[ch];
        for qu in first_qu..usize::from(SUBBAND_TO_QU[sb + 1]) {
            let wordlen = chan.qu_wordlen[qu];
            if wordlen <= 0 {
                continue;
            }

            let qu_lev = SF_TAB[chan.qu_sf_idx[qu] as usize] * MANT_TAB[wordlen as usize]
                / (1u32 << wordlen) as f32
                * grp_lev;

            let start = usize::from(QU_TO_SPEC_POS[qu]);
            let end = usize::from(QU_TO_SPEC_POS[qu + 1]);

            // Every quantization unit takes its noise from the start of the subband's noise
            // spectrum, not from the unit's offset in the subband.
            for (i, d) in self.spectrum[ch][start..end].iter_mut().enumerate() {
                let noise = NOISE_TAB[(rng_index + i) & 0x3ff];
                *d += noise * qu_lev;
            }
        }
    }

    /// IMDCT, gain compensation, tone synthesis and the inverse PQF.
    fn reconstruct_frame(&mut self, unit: &mut ChanUnit, num_channels: usize) {
        let cur = unit.cur;
        let num_subbands = unit.num_subbands;

        for ch in 0..num_channels {
            for sb in 0..num_subbands {
                let chan = &unit.channels[ch];
                let wind_id = (usize::from(chan.wnd_shape[cur ^ 1][sb]) << 1)
                    + usize::from(chan.wnd_shape[cur][sb]);

                // Inverse transform and windowing.
                let range = sb * SUBBAND_SAMPLES..(sb + 1) * SUBBAND_SAMPLES;
                self.imdct_in.copy_from_slice(&self.spectrum[ch][range.clone()]);
                self.imdct_window(wind_id, sb);

                // Gain compensation and overlapping.
                self.gain_compensation(
                    &chan.gain_data[cur ^ 1][sb],
                    &chan.gain_data[cur][sb],
                    &mut unit.prev_buf[ch][range.clone()],
                    range,
                );
            }

            // Zero unused subbands in both the output and the overlapping buffers.
            unit.prev_buf[ch][num_subbands * SUBBAND_SAMPLES..].fill(0.0);
            self.time_buf[num_subbands * SUBBAND_SAMPLES..].fill(0.0);

            // Resynthesize and add the tonal signal.
            if unit.wave_synth[cur].tones_present || unit.wave_synth[cur ^ 1].tones_present {
                for sb in 0..num_subbands {
                    let chan = &unit.channels[ch];
                    if chan.tones_info[cur][sb].num_wavs != 0
                        || chan.tones_info[cur ^ 1][sb].num_wavs != 0
                    {
                        self.generate_tones(unit, ch, sb);
                    }
                }
            }

            // Subband synthesis and acoustic signal output.
            self.ipqf(&mut unit.ipqf[ch], ch);
        }

        // Swap the window shape, gain control and tone buffers.
        unit.cur ^= 1;
    }

    /// Inverse MDCT of one subband followed by windowing; the result is left in `imdct_out`
    /// (FFmpeg's `ff_atrac3p_imdct`).
    fn imdct_window(&mut self, wind_id: usize, sb: usize) {
        if sb & 1 != 0 {
            self.imdct_in.reverse();
        }

        self.imdct.imdct(&self.imdct_in, &mut self.imdct_out);

        // ATRAC3+ uses two different MDCT windows:
        // - The first one is just the plain sine window of size 256.
        // - The 2nd one is the plain sine window of size 128 wrapped into zero (at the start)
        //   and one (at the end) regions. Both regions are 32 samples long.
        let t = self.tables;
        let out = &mut self.imdct_out;

        if wind_id & 2 != 0 {
            out[..32].fill(0.0);
            for (o, &w) in out[32..96].iter_mut().zip(&t.sine64) {
                *o *= w;
            }
        }
        else {
            for (o, &w) in out[..128].iter_mut().zip(&t.sine128) {
                *o *= w;
            }
        }

        if wind_id & 1 != 0 {
            for (o, &w) in out[160..224].iter_mut().zip(t.sine64.iter().rev()) {
                *o *= w;
            }
            out[224..].fill(0.0);
        }
        else {
            for (o, &w) in out[128..].iter_mut().zip(t.sine128.iter().rev()) {
                *o *= w;
            }
        }
    }

    /// Apply gain compensation and overlap-add the windowed IMDCT output in `imdct_out` into
    /// `time_buf[range]` (FFmpeg's `ff_atrac_gain_compensation`).
    fn gain_compensation(
        &mut self,
        gc_now: &GainInfo,
        gc_next: &GainInfo,
        prev: &mut [f32],
        range: std::ops::Range<usize>,
    ) {
        let t = self.tables;
        let input = &self.imdct_out;
        let out = &mut self.time_buf[range];

        let gc_scale =
            if gc_next.num_points != 0 { t.gain_tab1[gc_next.lev_code[0] as usize] } else { 1.0 };

        if gc_now.num_points == 0 {
            for (pos, o) in out.iter_mut().enumerate() {
                *o = input[pos] * gc_scale + prev[pos];
            }
        }
        else {
            let mut pos = 0;

            for i in 0..gc_now.num_points {
                let lastpos = (gc_now.loc_code[i] as usize) << LOC_SCALE;

                let mut lev = t.gain_tab1[gc_now.lev_code[i] as usize];
                let next_lev =
                    if i + 1 < gc_now.num_points { gc_now.lev_code[i + 1] } else { ID2EXP_OFFSET };
                let gain_inc = t.gain_tab2[(next_lev - gc_now.lev_code[i] + 15) as usize];

                // Apply a constant gain level and overlap.
                while pos < lastpos {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    pos += 1;
                }

                // Interpolate between two different gain levels.
                while pos < lastpos + LOC_SIZE {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    lev *= gain_inc;
                    pos += 1;
                }
            }

            while pos < out.len() {
                out[pos] = input[pos] * gc_scale + prev[pos];
                pos += 1;
            }
        }

        // Copy the overlapping part into the delay buffer.
        prev.copy_from_slice(&input[SUBBAND_SAMPLES..]);
    }

    /// Resynthesize the sine waves of one subband and add them to the subband signal
    /// (FFmpeg's `ff_atrac3p_generate_tones`).
    fn generate_tones(&mut self, unit: &mut ChanUnit, ch: usize, sb: usize) {
        let t = self.tables;
        let cur = unit.cur;
        let mut wavreg1 = [0.0f32; 128];
        let mut wavreg2 = [0.0f32; 128];

        let tones_now = unit.channels[ch].tones_info[cur ^ 1][sb];
        let mut tones_next = unit.channels[ch].tones_info[cur][sb];

        // Reconstruct full envelopes for both overlapping regions from truncated bitstream data.
        if tones_next.pend_env.has_start_point
            && tones_next.pend_env.start_pos < tones_next.pend_env.stop_pos
        {
            tones_next.curr_env.has_start_point = true;
            tones_next.curr_env.start_pos = tones_next.pend_env.start_pos + 32;
        }
        else if tones_now.pend_env.has_start_point {
            tones_next.curr_env.has_start_point = true;
            tones_next.curr_env.start_pos = tones_now.pend_env.start_pos;
        }
        else {
            tones_next.curr_env.has_start_point = false;
            tones_next.curr_env.start_pos = 0;
        }

        if tones_now.pend_env.has_stop_point
            && tones_now.pend_env.stop_pos >= tones_next.curr_env.start_pos
        {
            tones_next.curr_env.has_stop_point = true;
            tones_next.curr_env.stop_pos = tones_now.pend_env.stop_pos;
        }
        else if tones_next.pend_env.has_stop_point {
            tones_next.curr_env.has_stop_point = true;
            tones_next.curr_env.stop_pos = tones_next.pend_env.stop_pos + 32;
        }
        else {
            tones_next.curr_env.has_stop_point = false;
            tones_next.curr_env.stop_pos = 64;
        }

        // Is the visible part of the envelope non-zero?
        let reg1_env_nonzero = tones_now.curr_env.stop_pos >= 32;
        let reg2_env_nonzero = tones_next.curr_env.start_pos < 32;

        // Synthesize waves for both overlapping regions.
        if tones_now.num_wavs != 0 && reg1_env_nonzero {
            let ws = &unit.wave_synth[cur ^ 1];
            waves_synth(t, ws, &tones_now, ws.invert_phase[sb] & ch as u8 != 0, 128, &mut wavreg1);
        }

        if tones_next.num_wavs != 0 && reg2_env_nonzero {
            let ws = &unit.wave_synth[cur];
            waves_synth(t, ws, &tones_next, ws.invert_phase[sb] & ch as u8 != 0, 0, &mut wavreg2);
        }

        // Hann windowing for non-faded wave signals.
        if tones_now.num_wavs != 0
            && tones_next.num_wavs != 0
            && reg1_env_nonzero
            && reg2_env_nonzero
        {
            for (v, &w) in wavreg1.iter_mut().zip(&t.hann[128..]) {
                *v *= w;
            }
            for (v, &w) in wavreg2.iter_mut().zip(&t.hann[..128]) {
                *v *= w;
            }
        }
        else {
            if tones_now.num_wavs != 0 && !tones_now.curr_env.has_stop_point {
                for (v, &w) in wavreg1.iter_mut().zip(&t.hann[128..]) {
                    *v *= w;
                }
            }

            if tones_next.num_wavs != 0 && !tones_next.curr_env.has_start_point {
                for (v, &w) in wavreg2.iter_mut().zip(&t.hann[..128]) {
                    *v *= w;
                }
            }
        }

        // Overlap and add to the residual. Symphonia's IMDCT has the opposite sign convention to
        // FFmpeg's and the inverse PQF's IMDCT-IV cancels it for the spectral signal, so the tonal
        // signal, which enters after the IMDCT, is subtracted to keep its polarity.
        let out = &mut self.time_buf[sb * SUBBAND_SAMPLES..(sb + 1) * SUBBAND_SAMPLES];
        for (i, o) in out.iter_mut().enumerate() {
            *o -= wavreg1[i] + wavreg2[i];
        }

        // The reconstructed envelope of this frame is needed when the next frame is processed.
        unit.channels[ch].tones_info[cur][sb].curr_env = tones_next.curr_env;
    }

    /// Inverse polyphase quadrature filter bank: convert the 16 subband signals in `time_buf` to
    /// PCM in `output[ch]` (FFmpeg's `ff_atrac3p_ipqf`).
    fn ipqf(&mut self, hist: &mut IpqfState, ch: usize) {
        let out = &mut self.output[ch];
        out.fill(0.0);

        let mut idct_in = [0.0f32; SUBBANDS];
        let mut idct_out = [0.0f32; 2 * SUBBANDS];

        for s in 0..SUBBAND_SAMPLES {
            // Pick up one sample from each subband.
            for (sb, v) in idct_in.iter_mut().enumerate() {
                *v = self.time_buf[sb * SUBBAND_SAMPLES + s];
            }

            // Calculate the sine and cosine part of the PQF using an IDCT-IV. The wanted 16 values
            // are the middle of the full IMDCT output.
            self.ipqf_imdct.imdct(&idct_in, &mut idct_out);
            let idct_out = &idct_out[SUBBANDS / 2..SUBBANDS / 2 + SUBBANDS];

            // Append the result to the history.
            for i in 0..8 {
                hist.buf1[hist.pos][i] = idct_out[i + 8];
                hist.buf2[hist.pos][i] = idct_out[7 - i];
            }

            let mut pos_now = hist.pos;
            let mut pos_next = MOD23_LUT[pos_now + 2];

            let dst = &mut out[s * 16..s * 16 + 16];
            for t in 0..PQF_FIR_LEN {
                for i in 0..8 {
                    dst[i] += hist.buf1[pos_now][i] * IPQF_COEFFS1[t][i]
                        + hist.buf2[pos_next][i] * IPQF_COEFFS2[t][i];
                    dst[i + 8] += hist.buf1[pos_now][7 - i] * IPQF_COEFFS1[t][i + 8]
                        + hist.buf2[pos_next][7 - i] * IPQF_COEFFS2[t][i + 8];
                }

                pos_now = MOD23_LUT[pos_next + 2];
                pos_next = MOD23_LUT[pos_now + 2];
            }

            hist.pos = MOD23_LUT[hist.pos];
        }
    }
}

/// Synthesize the sine waves of one subband (FFmpeg's `waves_synth`).
///
/// `reg_offset` is the region offset for trimming the envelope data.
fn waves_synth(
    t: &Tables,
    synth: &WaveSynthParams,
    data: &WavesData,
    invert_phase: bool,
    reg_offset: i32,
    out: &mut [f32; 128],
) {
    let waves =
        synth.waves.get(data.start_index..data.start_index + data.num_wavs).unwrap_or_default();

    for wave in waves {
        // Amplitude dequantization (amplitude mode 1).
        let amp = f64::from(t.amp_sf[wave.amp_sf as usize]);

        let inc = wave.freq_index;
        let mut pos =
            ((((wave.phase_index & 0x1f) << 6) - (reg_offset ^ 128) * inc) & 2047) as usize;

        // Waveform generation.
        for o in out.iter_mut() {
            *o = (f64::from(*o) + f64::from(t.sine[pos]) * amp) as f32;
            pos = (pos + inc as usize) & 2047;
        }
    }

    // Invert the phase if requested.
    if invert_phase {
        for o in out.iter_mut() {
            *o *= -1.0;
        }
    }

    let envelope = &data.curr_env;

    // Fade in with a steep Hann window if requested.
    if envelope.has_start_point {
        let pos = (envelope.start_pos << 2) - reg_offset;
        if pos > 0 && pos <= 128 {
            let pos = pos as usize;
            out[..pos].fill(0.0);
            if !envelope.has_stop_point || envelope.start_pos != envelope.stop_pos {
                for (k, w) in [0, 32, 64, 96].into_iter().enumerate() {
                    if let Some(o) = out.get_mut(pos + k) {
                        *o *= t.hann[w];
                    }
                }
            }
        }
    }

    // Fade out with a steep Hann window if requested.
    if envelope.has_stop_point {
        let pos = ((envelope.stop_pos + 1) << 2) - reg_offset;
        if pos > 0 && pos <= 128 {
            let pos = pos as usize;
            for (k, w) in [96, 64, 32, 0].into_iter().enumerate() {
                if let Some(o) = (pos + k).checked_sub(4).and_then(|i| out.get_mut(i)) {
                    *o *= t.hann[w];
                }
            }
            out[pos..].fill(0.0);
        }
    }
}
