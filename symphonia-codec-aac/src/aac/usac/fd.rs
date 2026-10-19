// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The frequency-domain channel coder of USAC (`fd_channel_stream()`, ISO/IEC 23003-3 7.3).

use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::dsp::window::{WindowType, generate_window};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use super::arith::ArithState;
use super::lpd::{LpdDecoder, read_fac};
use super::tables::{
    NOISE_FILL_START_OFFSET, NUM_SWB_96, NUM_SWB_128, NUM_SWB_768, NUM_SWB_1024, SWB_OFFSET_96,
    SWB_OFFSET_128, SWB_OFFSET_768, SWB_OFFSET_1024, TNS_MAX_BANDS_128, TNS_MAX_BANDS_1024,
};
use super::tns::Tns;
use crate::aac::codebooks;
use crate::aac::dsp::ImdctAny;
use crate::aac::ics::pow43;

/// `window_sequence` values of `ics_info()` (ISO/IEC 23003-3 Table 65, via ISO/IEC 14496-3
/// 4.6.8.3.1).
pub const ONLY_LONG: u8 = 0;
pub const LONG_START: u8 = 1;
pub const EIGHT_SHORT: u8 = 2;
pub const LONG_STOP: u8 = 3;

/// The maximum number of scale factor bands of a window group.
const MAX_BANDS: usize = 64;

/// `ics_info()` of a channel: the window sequence and shape, the number of scale factor bands
/// and the window grouping of a short sequence.
#[derive(Clone)]
pub struct IcsInfo {
    pub window_sequence: u8,
    pub window_sequence_prev: u8,
    pub window_shape: bool,
    pub window_shape_prev: bool,
    pub max_sfb: usize,
    pub num_windows: usize,
    pub num_window_groups: usize,
    pub group_len: [usize; 8],
    pub swb_offset: &'static [u16],
    pub num_swb: usize,
    pub tns_max_bands: usize,
    /// The 7-bit `scale_factor_grouping` of a short sequence.
    grouping: u8,
}

impl IcsInfo {
    fn new() -> Self {
        IcsInfo {
            window_sequence: 0,
            window_sequence_prev: 0,
            window_shape: false,
            window_shape_prev: false,
            max_sfb: 0,
            num_windows: 1,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: SWB_OFFSET_1024[0],
            num_swb: 0,
            tns_max_bands: 0,
            grouping: 0,
        }
    }

    /// Read the window sequence and shape, rolling the current values into the previous ones.
    pub(super) fn read_window_header(&mut self, bs: &mut BitReaderLtr<'_>) -> Result<()> {
        self.window_sequence_prev = self.window_sequence;
        self.window_shape_prev = self.window_shape;

        self.window_sequence = bs.read_bits_leq32(2)? as u8;
        self.window_shape = bs.read_bool()?;

        Ok(())
    }

    /// Read `max_sfb` (and `scale_factor_grouping` of a short sequence).
    pub(super) fn read_max_sfb(&mut self, bs: &mut BitReaderLtr<'_>) -> Result<()> {
        if self.window_sequence == EIGHT_SHORT {
            self.max_sfb = bs.read_bits_leq32(4)? as usize;
            self.grouping = bs.read_bits_leq32(7)? as u8;
        }
        else {
            self.max_sfb = bs.read_bits_leq32(6)? as usize;
        }

        Ok(())
    }

    /// Read only `max_sfb`, reusing the already-shared window grouping.
    pub(super) fn read_max_sfb_only(&mut self, bs: &mut BitReaderLtr<'_>) -> Result<()> {
        self.max_sfb = if self.window_sequence == EIGHT_SHORT {
            bs.read_bits_leq32(4)? as usize
        }
        else {
            bs.read_bits_leq32(6)? as usize
        };

        Ok(())
    }

    /// Set up the scale factor band tables and the window grouping from the fields read so far.
    pub(super) fn setup(&mut self, ccfl: usize, sampling_index: usize) -> Result<()> {
        if self.window_sequence == EIGHT_SHORT {
            self.swb_offset = if ccfl == 768 {
                SWB_OFFSET_96[sampling_index]
            }
            else {
                SWB_OFFSET_128[sampling_index]
            };
            self.num_swb =
                if ccfl == 768 { NUM_SWB_96[sampling_index] } else { NUM_SWB_128[sampling_index] }
                    as usize;
            self.tns_max_bands = TNS_MAX_BANDS_128[sampling_index] as usize;

            // The 7-bit grouping mask: a set bit joins a window to the previous one.
            self.num_window_groups = 0;
            for j in 0..7 {
                self.group_len[j] = 1;
                if self.grouping & (1 << (6 - j)) != 0 {
                    self.group_len[self.num_window_groups] += 1;
                }
                else {
                    self.num_window_groups += 1;
                }
            }
            self.group_len[7] = 1;
            self.num_window_groups += 1;
            self.num_windows = 8;
        }
        else {
            self.swb_offset = if ccfl == 768 {
                SWB_OFFSET_768[sampling_index]
            }
            else {
                SWB_OFFSET_1024[sampling_index]
            };
            self.num_swb = if ccfl == 768 {
                NUM_SWB_768[sampling_index]
            }
            else {
                NUM_SWB_1024[sampling_index]
            } as usize;
            self.tns_max_bands = TNS_MAX_BANDS_1024[sampling_index] as usize;

            self.group_len[0] = 1;
            self.num_window_groups = 1;
            self.num_windows = 1;
        }

        if self.max_sfb > self.num_swb {
            return decode_error("aac: usac max_sfb exceeds the band count");
        }

        Ok(())
    }

    /// Read `ics_info()` and set up the window grouping and the scale factor band tables.
    pub(super) fn read(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        ccfl: usize,
        sampling_index: usize,
    ) -> Result<()> {
        self.read_window_header(bs)?;
        self.read_max_sfb(bs)?;
        self.setup(ccfl, sampling_index)
    }

    /// The number of spectral lines of a window.
    pub(super) fn window_len(&self, ccfl: usize) -> usize {
        if self.window_sequence == EIGHT_SHORT { ccfl / 8 } else { ccfl }
    }
}

/// The persistent state of one frequency-domain coded channel.
pub struct FdChannelState {
    pub coeffs: Vec<f32>,
    pub prev_coeffs: Vec<f32>,
    pub delay: Vec<f32>,
    /// The unwindowed second half of the last long IMDCT: the overlap a following LPD frame
    /// windows itself.
    pub delay_raw: Vec<f32>,
    pub sfo: [i16; 8 * MAX_BANDS],
    pub ics: IcsInfo,
    pub tns: Tns,
    pub arith: ArithState,
    pub noise_seed: u32,
    /// The seed that `reset()` restores `noise_seed` to.
    initial_noise_seed: u32,
    /// The `fac_data()` of the frame (`fac_len` values and the gain index), if present.
    pub fac_present: bool,
    pub fac_gain: i32,
    pub fac: [i32; 128],
    ccfl: usize,
    sampling_index: usize,
}

impl FdChannelState {
    pub fn new(ccfl: usize, sampling_index: usize, noise_seed: u32) -> Self {
        FdChannelState {
            coeffs: vec![0.0; 1024],
            prev_coeffs: vec![0.0; 1024],
            delay: vec![0.0; 1024],
            delay_raw: vec![0.0; 1024],
            sfo: [0; 8 * MAX_BANDS],
            ics: IcsInfo::new(),
            tns: Tns::new(),
            arith: ArithState::new(),
            noise_seed,
            initial_noise_seed: noise_seed,
            fac_present: false,
            fac_gain: 0,
            fac: [0; 128],
            ccfl,
            sampling_index,
        }
    }

    pub fn reset(&mut self) {
        self.coeffs.fill(0.0);
        self.prev_coeffs.fill(0.0);
        self.delay.fill(0.0);
        self.delay_raw.fill(0.0);
        self.sfo = [0; 8 * MAX_BANDS];
        self.ics = IcsInfo::new();
        self.tns.reset();
        self.arith.reset();
        self.noise_seed = self.initial_noise_seed;
        self.fac_present = false;
        self.fac_gain = 0;
        self.fac = [0; 128];
    }

    /// Parse `fd_channel_stream()` and dequantize the spectrum into `self.coeffs`.
    ///
    /// `ics_override` carries the `ics_info()` of a `common_window` channel pair, read once for
    /// both channels. `tns_data_present` is the per-channel TNS flag decoded by the caller (for a
    /// `common_window` pair the flag comes from the shared `common_tns`/`tns_on_lr` bits).
    pub fn parse(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        indep_flag: bool,
        noise_fill: bool,
        tns_data_present: bool,
        ics_override: Option<&IcsInfo>,
    ) -> Result<()> {
        let ccfl = self.ccfl;

        let global_gain = bs.read_bits_leq32(8)? as i16;

        let (noise_level, noise_offset) = if noise_fill {
            (bs.read_bits_leq32(3)? as i32, bs.read_bits_leq32(5)? as i32)
        }
        else {
            (0, 0)
        };

        if let Some(info) = ics_override {
            self.ics = info.clone();
        }
        else {
            self.ics.read(bs, ccfl, self.sampling_index)?;
        }

        let ics = self.ics.clone();

        // Scale factors: the first value is `global_gain`, the rest are Huffman-coded deltas.
        self.decode_scale_factors(bs, global_gain)?;

        if tns_data_present {
            self.tns.set_present();
            self.tns.read_data(bs, ics.num_windows, ics.window_sequence == EIGHT_SHORT)?;
        }
        else {
            self.tns.reset();
        }

        // ac_spectral_data()
        let arith_reset = if indep_flag { true } else { bs.read_bool()? };

        let window_len = ics.window_len(ccfl);
        let num_windows = ics.num_windows;

        self.coeffs[..1024].fill(0.0);

        for win in 0..num_windows {
            let lg = ics.swb_offset[ics.max_sfb] as usize;

            let n = window_len;

            let mut spec = [0i32; 1024];
            self.arith.decode_spectrum(bs, arith_reset && win == 0, lg, n, &mut spec[..n])?;

            // Dequantize: `|q|^(4/3)`, the scale factors are applied afterwards.
            let dst = &mut self.coeffs[win * window_len..(win + 1) * window_len];
            for (d, &q) in dst.iter_mut().zip(&spec[..n]) {
                *d = pow43(q as f32);
            }
        }

        // fac_data()
        self.fac_present = bs.read_bool()?;

        if self.fac_present {
            let fac_len = if ics.window_sequence == EIGHT_SHORT { ccfl / 16 } else { ccfl / 8 };
            self.fac_gain = bs.read_bits_leq32(7)? as i32;
            read_fac(bs, &mut self.fac[..fac_len])?;
        }

        // Spectral noise filling and the scale factor application (ISO/IEC 23003-3 7.5.1.4).
        self.spectrum_scale(noise_fill, noise_level, noise_offset);

        Ok(())
    }

    fn decode_scale_factors(&mut self, bs: &mut BitReaderLtr<'_>, global_gain: i16) -> Result<()> {
        let ics = &self.ics;

        let mut offset_sf = global_gain;

        for g in 0..ics.num_window_groups {
            for sfb in 0..ics.max_sfb {
                if g != 0 || sfb != 0 {
                    let huff = bs.read_codebook(&codebooks::SCALEFACTORS)?.0;
                    offset_sf += huff as i16 - 60;
                }

                if !(0..=255).contains(&offset_sf) {
                    return decode_error("aac: usac scale factor out of range");
                }

                self.sfo[g * ics.max_sfb + sfb] = offset_sf - 100;
            }
        }

        Ok(())
    }

    /// Spectral noise filling and the application of the scale factors.
    fn spectrum_scale(&mut self, noise_fill: bool, noise_level: i32, noise_offset: i32) {
        let ics = self.ics.clone();

        // A noise level of 0 injects no noise (libxaac).
        let noise_val =
            if noise_level == 0 { 0.0 } else { 2.0f32.powf((noise_level - 14) as f32 / 3.0) };
        let noise_offset = noise_offset - 16;

        let short_frame = usize::from(self.ccfl == 768);
        let is_eight_short = usize::from(ics.window_sequence == EIGHT_SHORT);
        let band_off = NOISE_FILL_START_OFFSET[short_frame][is_eight_short];

        let stride = ics.window_len(self.ccfl);

        // The cumulative scale factor, recomputed from `sfo` into the multiplicative domain.
        // `sfo` is `offset_sf - 100`; the scale is `2^(0.25 (offset_sf - 56))` (the 56, rather
        // than 100, accounts for the `1 / (2 n)` IMDCT scaling, exactly as the AAC-LC filterbank
        // of this crate does).
        let mut sf = [0.0f32; 8 * MAX_BANDS];
        for g in 0..ics.num_window_groups {
            for sfb in 0..ics.max_sfb {
                sf[g * ics.max_sfb + sfb] =
                    2.0f32.powf(0.25 * (self.sfo[g * ics.max_sfb + sfb] as f32 + 4.0));
            }
        }

        let mut win_tot = 0;

        for g in 0..ics.num_window_groups {
            let g_len = ics.group_len[g];

            for sfb in 0..ics.max_sfb {
                let off = ics.swb_offset[sfb] as usize;
                let cb_len = ics.swb_offset[sfb + 1] as usize - off;
                let idx = g * ics.max_sfb + sfb;

                if noise_fill {
                    let mut band_quantized_to_zero = true;

                    for group in 0..g_len {
                        let base = (win_tot + group) * stride + off;
                        if self.coeffs[base..base + cb_len].iter().any(|&c| c != 0.0) {
                            band_quantized_to_zero = false;
                        }
                    }

                    // The scale factor of a band that is entirely zero is raised by the noise
                    // offset; the noise is only injected above the start offset.
                    if band_quantized_to_zero {
                        sf[idx] =
                            2.0f32.powf(0.25 * (self.sfo[idx] as f32 + noise_offset as f32 + 4.0));
                    }

                    if off >= band_off {
                        for group in 0..g_len {
                            let base = (win_tot + group) * stride + off;
                            for c in &mut self.coeffs[base..base + cb_len] {
                                if *c == 0.0 {
                                    *c = noise_random_sign(&mut self.noise_seed) * noise_val;
                                }
                            }
                        }
                    }
                }

                let scale = sf[idx];
                for group in 0..g_len {
                    let base = (win_tot + group) * stride + off;
                    for c in &mut self.coeffs[base..base + cb_len] {
                        *c *= scale;
                    }
                }
            }

            win_tot += g_len;
        }
    }
}

/// The spectral noise filling sign generator (ISO/IEC 23003-3 7.5.1.4).
fn noise_random_sign(seed: &mut u32) -> f32 {
    let new_seed = seed.wrapping_mul(69069).wrapping_add(5);
    *seed = new_seed;
    if new_seed & 0x1_0000 != 0 { -1.0 } else { 1.0 }
}

/// The IMDCT of a USAC window length: 1024/128 use the core FFT-based transform, 768/96 the
/// arbitrary-size one.
enum FdImdct {
    Fft(Imdct),
    Any(ImdctAny),
}

impl FdImdct {
    fn new(n: usize) -> Self {
        if n.is_power_of_two() {
            FdImdct::Fft(Imdct::new_scaled(n, 1.0 / (2 * n) as f64))
        }
        else {
            FdImdct::Any(ImdctAny::new(n, 1.0 / (2 * n) as f64))
        }
    }

    fn imdct(&mut self, spec: &[f32], out: &mut [f32]) {
        match self {
            FdImdct::Fft(imdct) => imdct.imdct(spec, out),
            FdImdct::Any(imdct) => imdct.imdct(spec, out),
        }
    }
}

/// The filterbank of the frequency-domain coder: IMDCT, windowing and overlap-add for a core
/// frame length of 768 or 1024 samples.
pub struct FdFilterbank {
    ccfl: usize,
    short: usize,
    imdct_long: FdImdct,
    imdct_short: FdImdct,
    sine_long: Vec<f32>,
    kbd_long: Vec<f32>,
    sine_short: Vec<f32>,
    kbd_short: Vec<f32>,
    buf: Vec<f32>,
    pcm_short: Vec<f32>,
}

impl FdFilterbank {
    pub fn new(ccfl: usize) -> Self {
        let short = ccfl / 8;

        let mut sine_long = vec![0.0; ccfl];
        let mut kbd_long = vec![0.0; ccfl];
        let mut sine_short = vec![0.0; short];
        let mut kbd_short = vec![0.0; short];

        generate_window(WindowType::Sine, 1.0, ccfl, true, &mut sine_long);
        generate_window(WindowType::KaiserBessel(4.0), 1.0, ccfl, true, &mut kbd_long);
        generate_window(WindowType::Sine, 1.0, short, true, &mut sine_short);
        generate_window(WindowType::KaiserBessel(6.0), 1.0, short, true, &mut kbd_short);

        FdFilterbank {
            ccfl,
            short,
            imdct_long: FdImdct::new(ccfl),
            imdct_short: FdImdct::new(short),
            sine_long,
            kbd_long,
            sine_short,
            kbd_short,
            buf: vec![0.0; 2 * ccfl],
            pcm_short: vec![0.0; ccfl + short],
        }
    }

    /// Synthesize one channel: IMDCT, window and overlap-add, writing `ccfl` time-domain samples
    /// (at the core sample rate, on the INT16 scale) into `out`.
    ///
    /// `after_lpd` is the LPD decoder of the channel when the previous frame was LPD coded: the
    /// frame then starts from its state and the transition window and the FAC apply.
    pub fn synth(
        &mut self,
        state: &mut FdChannelState,
        out: &mut [f32],
        mut after_lpd: Option<&mut LpdDecoder>,
    ) -> Result<()> {
        let ics = &state.ics;
        let ccfl = self.ccfl;
        let short = self.short;
        let half_short = short / 2;
        let point0 = ccfl / 2 - half_short;
        let point1 = ccfl / 2 + half_short;

        let (long_win, short_win) = if ics.window_shape {
            (&self.kbd_long, &self.kbd_short)
        }
        else {
            (&self.sine_long, &self.sine_short)
        };

        let (prev_long_win, prev_short_win) = if ics.window_shape_prev {
            (&self.kbd_long, &self.kbd_short)
        }
        else {
            (&self.sine_long, &self.sine_short)
        };

        let seq = ics.window_sequence;

        // A long start window after a start, short or stop-start window (or an LPD frame, which
        // leaves EIGHT_SHORT as the previous sequence) is a STOP_START window: it has the left
        // half of a stop window and the right half of a start window (ISO/IEC 23003-3 and
        // libxaac `ixheaacd_win_seq_select`).
        let stop_start =
            seq == LONG_START && matches!(ics.window_sequence_prev, LONG_START | EIGHT_SHORT);
        let left_stop = seq == LONG_STOP || stop_start;

        if let Some(lpd) = after_lpd.as_deref_mut() {
            if seq == ONLY_LONG {
                return unsupported_error("aac: usac long window directly after an lpd frame");
            }

            lpd.dec_update(&mut state.delay[..ccfl]);
        }

        if seq != EIGHT_SHORT {
            self.imdct_long.imdct(&state.coeffs[..ccfl], &mut self.buf);
        }
        else {
            // Eight short IMDCTs, then windowed and concatenated into `pcm_short`.
            for (win, src) in state.coeffs[..ccfl].chunks_exact(short).enumerate() {
                let out_start = win * 2 * short;
                self.imdct_short.imdct(src, &mut self.buf[out_start..out_start + 2 * short]);
            }

            self.pcm_short.fill(0.0);

            for (win, src) in self.buf[..8 * 2 * short].chunks_exact(2 * short).enumerate() {
                if win > 0 {
                    for i in 0..short {
                        self.pcm_short[win * short + i] += src[i] * short_win[i];
                        self.pcm_short[win * short + i + short] +=
                            src[i + short] * short_win[short - 1 - i];
                    }
                }
                else {
                    for i in 0..short {
                        self.pcm_short[i] = src[i] * prev_short_win[i];
                        self.pcm_short[i + short] = src[i + short] * short_win[short - 1 - i];
                    }
                }
            }
        }

        // Output new samples by overlap-adding the windowed IMDCT with the saved tail.
        match seq {
            ONLY_LONG | LONG_START if !stop_start => {
                for i in 0..ccfl {
                    out[i] = state.delay[i] + self.buf[i] * prev_long_win[i];
                }
            }
            EIGHT_SHORT if after_lpd.is_some() => {
                // libxaac `ixheaacd_windowing_short1/2` and `combine_fac`.
                let lfac = ccfl / 16;
                let mut fac = [0.0f32; 128 * 2];
                out[..point0].copy_from_slice(&state.delay[..point0]);

                if state.fac_present {
                    if let Some(lpd) = after_lpd.as_deref_mut() {
                        lpd.fac_for_fd(state.fac_gain, &state.fac[..lfac], lfac, &mut fac);
                    }

                    out[point0..point0 + lfac].copy_from_slice(&state.delay[point0..point0 + lfac]);
                    out[point0 + lfac..ccfl].copy_from_slice(&self.pcm_short[lfac..ccfl - point0]);
                    for k in 0..2 * lfac {
                        out[ccfl / 2 + k] += fac[k];
                    }
                }
                else {
                    for i in 0..short {
                        out[point0 + i] = state.delay[point0 + i] * prev_short_win[short - 1 - i]
                            + self.pcm_short[i];
                    }
                    out[point0 + short..ccfl]
                        .copy_from_slice(&self.pcm_short[short..ccfl - point0]);
                }
            }
            EIGHT_SHORT => {
                out[..point0].copy_from_slice(&state.delay[..point0]);
                for i in point0..ccfl {
                    out[i] = state.delay[i] + self.pcm_short[i - point0];
                }
            }
            _ if left_stop && after_lpd.is_some() => {
                let lfac = ccfl / 8;
                let ntrans = 2 * lfac;
                let nflat = ccfl / 2 - lfac;
                let mut win = vec![0.0f32; ntrans];
                generate_window(WindowType::Sine, 1.0, ntrans, true, &mut win);

                if state.fac_present {
                    let mut fac = [0.0f32; 128 * 2];

                    if let Some(lpd) = after_lpd.as_deref_mut() {
                        lpd.fac_for_fd(state.fac_gain, &state.fac[..lfac], lfac, &mut fac);
                    }

                    out[..ccfl / 2].copy_from_slice(&state.delay[..ccfl / 2]);
                    for i in 0..lfac {
                        out[ccfl / 2 + i] = self.buf[ccfl / 2 + i] * win[lfac + i] + fac[i];
                        out[ccfl / 2 + lfac + i] = self.buf[ccfl / 2 + lfac + i] + fac[lfac + i];
                    }
                    out[ccfl / 2 + 2 * lfac..ccfl]
                        .copy_from_slice(&self.buf[ccfl / 2 + 2 * lfac..ccfl]);
                }
                else {
                    out[..nflat].copy_from_slice(&state.delay[..nflat]);
                    for i in nflat..nflat + ntrans {
                        out[i] = self.buf[i] * win[i - nflat]
                            + state.delay[i] * win[ntrans - 1 - (i - nflat)];
                    }
                    out[nflat + ntrans..ccfl].copy_from_slice(&self.buf[nflat + ntrans..ccfl]);
                }
            }
            _ if left_stop => {
                out[..point0].copy_from_slice(&state.delay[..point0]);
                for i in point0..point1 {
                    out[i] = state.delay[i] + self.buf[i] * prev_short_win[i - point0];
                }
                for i in point1..ccfl {
                    out[i] = state.delay[i] + self.buf[i];
                }
            }
            _ => unreachable!("window sequence is 0..3"),
        }

        if seq != EIGHT_SHORT {
            state.delay_raw[..ccfl].copy_from_slice(&self.buf[ccfl..2 * ccfl]);
        }

        // Save the windowed tail for the next frame.
        match seq {
            ONLY_LONG | LONG_STOP if !stop_start => {
                for i in 0..ccfl {
                    state.delay[i] = self.buf[i + ccfl] * long_win[ccfl - 1 - i];
                }
            }
            EIGHT_SHORT => {
                for i in 0..point1 {
                    state.delay[i] = self.pcm_short[i + point1];
                }
                state.delay[point1..ccfl].fill(0.0);

                // A following LPD frame windows the last short window's tail itself.
                state.delay_raw[..point0].copy_from_slice(&state.delay[..point0]);
                let last = 7 * 2 * short + short;
                state.delay_raw[point0..point1].copy_from_slice(&self.buf[last..last + short]);
                state.delay_raw[point1..ccfl].fill(0.0);
            }
            LONG_START => {
                state.delay[..point0].copy_from_slice(&self.buf[ccfl..ccfl + point0]);
                for i in point0..point1 {
                    state.delay[i] = self.buf[i + ccfl] * short_win[short - 1 - (i - point0)];
                }
                state.delay[point1..ccfl].fill(0.0);
            }
            _ => unreachable!("window sequence is 0..3"),
        }

        if let Some(lpd) = after_lpd {
            // libxaac `ixheaacd_fd_imdct_short` passes `is_short_flag` 1 and
            // `ixheaacd_fd_imdct_long` passes 0 to `ixheaacd_lpd_bpf_fix`.
            lpd.bpf_fix(seq == EIGHT_SHORT, out);
        }

        Ok(())
    }
}
