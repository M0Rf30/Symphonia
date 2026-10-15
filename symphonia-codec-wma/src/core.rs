// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block and frame decoder of WMA v1 and v2.
//!
//! This is a port of FFmpeg's `libavcodec/wma.c` and `libavcodec/wmadec.c` (LGPL-2.1-or-later, see
//! the `NOTICE` file). The names of the FFmpeg functions that a function is ported from are given
//! in its documentation. The arithmetic follows the reference, including the C type promotions
//! (`float` operations that are promoted to `double`), so that the output is equal to FFmpeg's up
//! to the rounding of the IMDCT.

use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::vlc::{
    BitOrder, Codebook, CodebookBuilder, Entry8x16, Entry8x32, Entry16x32,
};

use crate::bits::Bits;
use crate::tables::*;

/// The smallest log2 block size.
const BLOCK_MIN_BITS: usize = 7;
/// The largest log2 block size.
const BLOCK_MAX_BITS: usize = 11;
/// The number of block sizes.
const BLOCK_NB_SIZES: usize = BLOCK_MAX_BITS - BLOCK_MIN_BITS + 1;
/// The maximum number of channels.
pub const MAX_CHANNELS: usize = 2;
/// The number of bands of an exponent band table.
const MAX_BANDS: usize = 25;
/// The number of LSP coefficients.
const NB_LSP_COEFS: usize = 10;
/// The size of the noise table.
const NOISE_TAB_SIZE: usize = 8192;
/// The number of mantissa bits of the `x^-0.25` interpolation tables.
const LSP_POW_BITS: u32 = 7;
/// `MIN_CACHE_BITS` of FFmpeg's bit reader, the limit of the bit reservoir offset field.
const MIN_CACHE_BITS: u32 = 25;
/// The largest sample rate that FFmpeg accepts.
const MAX_SAMPLE_RATE: u32 = 50000;

/// The number of bits of the VLC of an exponent code (`EXPVLCBITS`) are irrelevant here; the
/// codebooks are built by the core `CodebookBuilder`.
const CODEBOOK_BITS: u8 = 9;

/// The `pow(10, i / 16.0)` table of `decode_exp_vlc` is computed for `i` in `-60..=95`.
const POW_TAB_LEN: usize = 156;
const POW_TAB_BIAS: i32 = 60;

/// A Huffman coded run/level table of the spectral coefficients (`init_coef_vlc`).
struct CoefTable {
    codebook: Codebook<Entry16x32>,
    /// The run of a code (`run_table`).
    run: Vec<u16>,
    /// The level of a code (`level_table`).
    level: Vec<f32>,
}

impl CoefTable {
    fn new(codes: &[u32], bits: &[u8], levels: &[u16]) -> Result<CoefTable> {
        let n = bits.len();
        let values: Vec<u16> = (0..n as u16).collect();

        let codebook = match CodebookBuilder::new(BitOrder::Verbatim)
            .bits_per_read(CODEBOOK_BITS)
            .make::<Entry16x32>(codes, bits, &values)
        {
            Ok(codebook) => codebook,
            Err(_) => return unsupported_error("wma: invalid coefficient codebook"),
        };

        let mut run = vec![0u16; n];
        let mut level = vec![0f32; n];

        // The codes 0 and 1 are the escape and the end of block codes. The others code a run and
        // a level: `levels[k]` is the number of codes of the level `k + 1`, with increasing runs.
        let mut i = 2;
        let mut lvl = 1.0f32;

        for &count in levels {
            if i >= n {
                break;
            }
            for j in 0..count {
                if i >= n {
                    break;
                }
                run[i] = j;
                level[i] = lvl;
                i += 1;
            }
            lvl += 1.0;
        }

        Ok(CoefTable { codebook, run, level })
    }
}

/// Builds a codebook from `(symbol, length)` pairs the way `ff_vlc_init_from_lengths` does: the
/// codes are assigned in the order of the table, each one following the previous.
fn codebook_from_lengths(
    lens: impl Iterator<Item = (u8, u8)>,
    nb_codes: usize,
) -> Result<Codebook<Entry8x16>> {
    let mut codes = Vec::with_capacity(nb_codes);
    let mut code_lens = Vec::with_capacity(nb_codes);
    let mut symbols = Vec::with_capacity(nb_codes);

    // The code, left-aligned in 32 bits.
    let mut code = 0u64;

    for (symbol, len) in lens {
        if len == 0 || len > 32 || code & ((1u64 << (32 - len)) - 1) != 0 {
            return unsupported_error("wma: invalid codebook lengths");
        }
        codes.push((code >> (32 - len)) as u32);
        code_lens.push(len);
        symbols.push(symbol);
        code += 1u64 << (32 - len);
        if code > u64::from(u32::MAX) + 1 {
            return unsupported_error("wma: overdetermined codebook");
        }
    }

    match CodebookBuilder::new(BitOrder::Verbatim)
        .bits_per_read(9)
        .make(&codes, &code_lens, &symbols)
    {
        Ok(codebook) => Ok(codebook),
        Err(_) => unsupported_error("wma: invalid codebook"),
    }
}

/// The parameters of a stream that the block decoder depends on.
#[derive(Copy, Clone, Debug)]
pub struct StreamParams {
    /// 1 for WMA v1 (format tag 0x0160), 2 for WMA v2 (0x0161).
    pub version: u8,
    pub sample_rate: u32,
    pub channels: usize,
    /// The bit rate in bits per second.
    pub bit_rate: u32,
    /// The codec flags of the extra data (`flags2`).
    pub flags: u16,
}

/// The decoder of the frames of a WMA v1/v2 stream (`WMACodecContext`).
pub struct Core {
    version: u8,
    channels: usize,
    use_exp_vlc: bool,
    pub use_bit_reservoir: bool,
    use_variable_block_len: bool,
    use_noise_coding: bool,
    pub byte_offset_bits: u32,

    exp_codebook: Option<Codebook<Entry8x32>>,
    hgain_codebook: Option<Codebook<Entry8x16>>,
    coef_tables: [CoefTable; 2],

    exponent_sizes: [usize; BLOCK_NB_SIZES],
    exponent_bands: [[u16; MAX_BANDS]; BLOCK_NB_SIZES],
    high_band_start: [usize; BLOCK_NB_SIZES],
    /// The first coded coefficient.
    coefs_start: usize,
    /// The maximum number of coded coefficients.
    coefs_end: [usize; BLOCK_NB_SIZES],
    exponent_high_sizes: [usize; BLOCK_NB_SIZES],
    exponent_high_bands: [[usize; MAX_BANDS]; BLOCK_NB_SIZES],

    high_band_coded: [[bool; MAX_BANDS]; MAX_CHANNELS],
    high_band_values: [[i32; MAX_BANDS]; MAX_CHANNELS],

    /// The frame length in samples.
    pub frame_len: usize,
    frame_len_bits: usize,
    nb_block_sizes: usize,

    pub reset_block_lengths: bool,
    block_len_bits: usize,
    next_block_len_bits: usize,
    prev_block_len_bits: usize,
    block_len: usize,
    block_pos: usize,
    ms_stereo: bool,
    channel_coded: [bool; MAX_CHANNELS],
    exponents_bsize: [usize; MAX_CHANNELS],
    exponents: [Vec<f32>; MAX_CHANNELS],
    max_exponent: [f32; MAX_CHANNELS],
    coefs1: [Vec<f32>; MAX_CHANNELS],
    coefs: [Vec<f32>; MAX_CHANNELS],
    /// The output of the last IMDCT.
    output: Vec<f32>,
    imdct: Vec<Imdct>,
    windows: Vec<Vec<f32>>,
    /// The overlap-add buffer of each channel: two frames.
    frame_out: [Vec<f32>; MAX_CHANNELS],
    exponents_initialized: [bool; MAX_CHANNELS],

    noise_table: Vec<f32>,
    noise_index: usize,
    noise_mult: f32,

    lsp_cos_table: Vec<f32>,
    lsp_pow_e_table: [f32; 256],
    lsp_pow_m_table1: [f32; 1 << LSP_POW_BITS],
    lsp_pow_m_table2: [f32; 1 << LSP_POW_BITS],
    pow_tab: [f32; POW_TAB_LEN],
}

/// `av_log2`: the floor of the base-2 logarithm, 0 for 0.
fn log2(x: u32) -> u32 {
    if x == 0 { 0 } else { 31 - x.leading_zeros() }
}

/// `ff_wma_get_frame_len_bits` for versions 1 and 2 (`wma_common.c`).
fn frame_len_bits(sample_rate: u32, version: u8) -> usize {
    if sample_rate <= 16000 {
        9
    }
    else if sample_rate <= 22050 || (sample_rate <= 32000 && version == 1) {
        10
    }
    else {
        11
    }
}

/// `ff_wma_total_gain_to_bits`: the number of bits of an escaped coefficient level.
fn total_gain_to_bits(total_gain: i32) -> u32 {
    if total_gain < 15 {
        13
    }
    else if total_gain < 32 {
        12
    }
    else if total_gain < 40 {
        11
    }
    else if total_gain < 45 {
        10
    }
    else {
        9
    }
}

impl Core {
    /// `ff_wma_init` and `wma_decode_init`.
    pub fn new(p: &StreamParams) -> Result<Core> {
        if p.sample_rate == 0 || p.sample_rate > MAX_SAMPLE_RATE {
            return unsupported_error("wma: unsupported sample rate");
        }
        if p.channels == 0 || p.channels > MAX_CHANNELS {
            return unsupported_error("wma: unsupported number of channels");
        }
        if p.bit_rate == 0 {
            return unsupported_error("wma: missing bit rate");
        }

        let version = p.version;
        let channels = p.channels;
        let sample_rate = p.sample_rate;
        let sr = sample_rate as usize;

        let use_exp_vlc = p.flags & 0x0001 != 0;
        let use_bit_reservoir = p.flags & 0x0002 != 0;
        let use_variable_block_len = p.flags & 0x0004 != 0;

        // Compute the MDCT block size.
        let frame_len_bits = frame_len_bits(sample_rate, version);
        let frame_len = 1usize << frame_len_bits;

        let nb_block_sizes = if use_variable_block_len {
            let mut nb = usize::from((p.flags >> 3) & 3) + 1;
            if p.bit_rate / channels as u32 >= 32000 {
                nb += 2;
            }
            let nb_max = frame_len_bits - BLOCK_MIN_BITS;
            nb.min(nb_max) + 1
        }
        else {
            1
        };

        // Initialise the rate dependent parameters.
        let mut use_noise_coding = true;
        let mut high_freq = (f64::from(sample_rate) * 0.5) as f32;
        let scale_hf = |hf: f32, k: f64| (f64::from(hf) * k) as f32;

        // With version 2, the rates are normalised.
        let sample_rate1 = if version == 2 {
            match sample_rate {
                44100.. => 44100,
                22050.. => 22050,
                16000.. => 16000,
                11025.. => 11025,
                8000.. => 8000,
                r => r,
            }
        }
        else {
            sample_rate
        };

        let bps = p.bit_rate as f32 / (channels as f32 * sample_rate as f32);
        let byte_offset_bits =
            log2(((f64::from(bps * frame_len as f32)) / 8.0 + 0.5) as i32 as u32) + 2;
        if byte_offset_bits + 3 > MIN_CACHE_BITS {
            return unsupported_error("wma: byte offset is too large");
        }

        // Compute the high frequency value and choose if noise coding is activated.
        let mut bps1 = bps;
        if channels == 2 {
            bps1 = (f64::from(bps) * 1.6) as f32;
        }
        let bps1d = f64::from(bps1);
        let bpsd = f64::from(bps);

        match sample_rate1 {
            44100 => {
                if bps1d >= 0.61 {
                    use_noise_coding = false;
                }
                else {
                    high_freq = scale_hf(high_freq, 0.4);
                }
            }
            22050 => {
                if bps1d >= 1.16 {
                    use_noise_coding = false;
                }
                else if bps1d >= 0.72 {
                    high_freq = scale_hf(high_freq, 0.7);
                }
                else {
                    high_freq = scale_hf(high_freq, 0.6);
                }
            }
            16000 => {
                if bpsd > 0.5 {
                    high_freq = scale_hf(high_freq, 0.5);
                }
                else {
                    high_freq = scale_hf(high_freq, 0.3);
                }
            }
            11025 => high_freq = scale_hf(high_freq, 0.7),
            8000 => {
                if bpsd <= 0.625 {
                    high_freq = scale_hf(high_freq, 0.5);
                }
                else if bpsd > 0.75 {
                    use_noise_coding = false;
                }
                else {
                    high_freq = scale_hf(high_freq, 0.65);
                }
            }
            _ => {
                if bpsd >= 0.8 {
                    high_freq = scale_hf(high_freq, 0.75);
                }
                else if bpsd >= 0.6 {
                    high_freq = scale_hf(high_freq, 0.6);
                }
                else {
                    high_freq = scale_hf(high_freq, 0.5);
                }
            }
        }

        // Compute the scale factor band sizes for each MDCT block size.
        let coefs_start = if version == 1 { 3 } else { 0 };
        let mut exponent_sizes = [0; BLOCK_NB_SIZES];
        let mut exponent_bands = [[0u16; MAX_BANDS]; BLOCK_NB_SIZES];
        let mut high_band_start = [0; BLOCK_NB_SIZES];
        let mut coefs_end = [0; BLOCK_NB_SIZES];
        let mut exponent_high_sizes = [0; BLOCK_NB_SIZES];
        let mut exponent_high_bands = [[0usize; MAX_BANDS]; BLOCK_NB_SIZES];

        for k in 0..nb_block_sizes {
            let block_len = frame_len >> k;

            if version == 1 {
                // FFmpeg stores the bands of every block size in the entry of the first, which
                // is only correct for streams that do not use variable block lengths (those of
                // the version 1 encoders). The entry of the block size is filled here.
                let mut lpos = 0;
                let mut i = 0;
                while i < MAX_BANDS {
                    let a = usize::from(CRITICAL_FREQS[i]);
                    let pos = (((block_len * 2 * a) + (sr >> 1)) / sr).min(block_len);
                    exponent_bands[k][i] = (pos - lpos) as u16;
                    if pos >= block_len {
                        i += 1;
                        break;
                    }
                    lpos = pos;
                    i += 1;
                }
                exponent_sizes[k] = i;
            }
            else {
                // Hardcoded tables.
                let a = frame_len_bits - BLOCK_MIN_BITS - k;
                let table = if a < 3 {
                    if sample_rate >= 44100 {
                        Some(&EXPONENT_BAND_44100[a])
                    }
                    else if sample_rate >= 32000 {
                        Some(&EXPONENT_BAND_32000[a])
                    }
                    else if sample_rate >= 22050 {
                        Some(&EXPONENT_BAND_22050[a])
                    }
                    else {
                        None
                    }
                }
                else {
                    None
                };

                if let Some(table) = table {
                    let n = usize::from(table[0]);
                    for i in 0..n {
                        exponent_bands[k][i] = u16::from(table[i + 1]);
                    }
                    exponent_sizes[k] = n;
                }
                else {
                    let mut j = 0;
                    let mut lpos = 0;
                    for i in 0..MAX_BANDS {
                        let a = usize::from(CRITICAL_FREQS[i]);
                        let mut pos = ((block_len * 2 * a) + (sr << 1)) / (4 * sr);
                        pos <<= 2;
                        let pos = pos.min(block_len);
                        if pos > lpos {
                            exponent_bands[k][j] = (pos - lpos) as u16;
                            j += 1;
                        }
                        if pos >= block_len {
                            break;
                        }
                        lpos = pos;
                    }
                    exponent_sizes[k] = j;
                }
            }

            // The bands must tile the block, otherwise the exponents would be incomplete.
            let total: usize =
                exponent_bands[k][..exponent_sizes[k]].iter().map(|&b| usize::from(b)).sum();
            if total != block_len {
                return unsupported_error("wma: invalid exponent bands");
            }

            // The maximum number of coefficients.
            coefs_end[k] = (frame_len - ((frame_len * 9) / 100)) >> k;

            // The high frequency computation.
            high_band_start[k] =
                ((((block_len * 2) as f32 * high_freq) / sample_rate as f32) as f64 + 0.5) as usize;
            let n = exponent_sizes[k];
            let mut j = 0;
            let mut pos = 0;
            for i in 0..n {
                let mut start = pos;
                pos += usize::from(exponent_bands[k][i]);
                let mut end = pos;
                if start < high_band_start[k] {
                    start = high_band_start[k];
                }
                if end > coefs_end[k] {
                    end = coefs_end[k];
                }
                if end > start {
                    exponent_high_bands[k][j] = end - start;
                    j += 1;
                }
            }
            exponent_high_sizes[k] = j;

            // The high bands are only used by the noise coding.
            if use_noise_coding && high_band_start[k] > coefs_end[k] {
                return unsupported_error("wma: invalid high band start");
            }
        }

        // Initialise the MDCT windows: simple sine windows (`ff_sine_window_init`), and the MDCTs
        // (scaled to the [-1, 1] range of the float output).
        let mut windows = Vec::with_capacity(nb_block_sizes);
        let mut imdct = Vec::with_capacity(nb_block_sizes);
        for i in 0..nb_block_sizes {
            let n = 1usize << (frame_len_bits - i);
            let window = (0..n)
                .map(|j| ((j as f64 + 0.5) / (2.0 * n as f64) * std::f64::consts::PI).sin() as f32)
                .collect();
            windows.push(window);
            // The sign of the transform of `Imdct` is opposite to the one of the reference.
            imdct.push(Imdct::new_scaled(n, -1.0 / 32768.0));
        }

        // The noise generator.
        let mut noise_mult = 0.0;
        let mut noise_table = Vec::new();
        if use_noise_coding {
            noise_mult = if use_exp_vlc { 0.02f32 } else { 0.04f32 };
            let norm = ((1.0 / 2147483648.0f64) * 3f64.sqrt() * f64::from(noise_mult)) as f32;
            let mut seed = 1u32;
            noise_table.reserve_exact(NOISE_TAB_SIZE);
            for _ in 0..NOISE_TAB_SIZE {
                seed = seed.wrapping_mul(314159).wrapping_add(1);
                noise_table.push((seed as i32) as f32 * norm);
            }
        }

        // Choose the VLC tables of the coefficients.
        let mut coef_vlc_table = 2;
        if sample_rate >= 32000 {
            if bps1d < 0.72 {
                coef_vlc_table = 0;
            }
            else if bps1d < 1.16 {
                coef_vlc_table = 1;
            }
        }
        let coef_tables = match coef_vlc_table {
            0 => [
                CoefTable::new(&COEF0_HUFFCODES, &COEF0_HUFFBITS, &LEVELS0)?,
                CoefTable::new(&COEF1_HUFFCODES, &COEF1_HUFFBITS, &LEVELS1)?,
            ],
            1 => [
                CoefTable::new(&COEF2_HUFFCODES, &COEF2_HUFFBITS, &LEVELS2)?,
                CoefTable::new(&COEF3_HUFFCODES, &COEF3_HUFFBITS, &LEVELS3)?,
            ],
            _ => [
                CoefTable::new(&COEF4_HUFFCODES, &COEF4_HUFFBITS, &LEVELS4)?,
                CoefTable::new(&COEF5_HUFFCODES, &COEF5_HUFFBITS, &LEVELS5)?,
            ],
        };

        // The high band gain codes, with the symbols offset by -18 when they are read.
        let hgain_codebook = if use_noise_coding {
            Some(codebook_from_lengths(
                HGAIN_HUFFTAB.iter().map(|&[symbol, len]| (symbol, len)),
                HGAIN_HUFFTAB.len(),
            )?)
        }
        else {
            None
        };

        // The exponent codes are those of the AAC scale factors.
        let mut lsp_cos_table = Vec::new();
        let exp_codebook = if use_exp_vlc {
            let values: Vec<u8> = (0..SCALEFACTOR_BITS.len() as u8).collect();
            match CodebookBuilder::new(BitOrder::Verbatim).bits_per_read(8).make::<Entry8x32>(
                &SCALEFACTOR_CODE,
                &SCALEFACTOR_BITS,
                &values,
            ) {
                Ok(codebook) => Some(codebook),
                Err(_) => return unsupported_error("wma: invalid exponent codebook"),
            }
        }
        else {
            // The tables of `wma_lsp_to_curve_init`.
            let wdel = std::f32::consts::PI / frame_len as f32;
            lsp_cos_table =
                (0..frame_len).map(|i| 2.0f32 * f64::from(wdel * i as f32).cos() as f32).collect();
            None
        };

        let mut lsp_pow_e_table = [0f32; 256];
        let mut lsp_pow_m_table1 = [0f32; 1 << LSP_POW_BITS];
        let mut lsp_pow_m_table2 = [0f32; 1 << LSP_POW_BITS];
        if !use_exp_vlc {
            // Tables for the `x^-0.25` computation.
            for (i, e) in lsp_pow_e_table.iter_mut().enumerate() {
                *e = ((i as i32 - 126) as f32 * -0.25).exp2();
            }

            // These two tables avoid two operations in `pow_m1_4`.
            let mut b = 1.0f32;
            for i in (0..(1usize << LSP_POW_BITS)).rev() {
                let m = (1usize << LSP_POW_BITS) + i;
                let a = m as f32 * (0.5 / (1u32 << LSP_POW_BITS) as f32);
                let a = (1.0 / f64::from(a).sqrt().sqrt()) as f32;
                lsp_pow_m_table1[i] = 2.0 * a - b;
                lsp_pow_m_table2[i] = b - a;
                b = a;
            }
        }

        // `pow(10, i / 16.0)` for `i` in `-60..=95`.
        let mut pow_tab = [0f32; POW_TAB_LEN];
        for (i, v) in pow_tab.iter_mut().enumerate() {
            *v = 10f64.powf(f64::from(i as i32 - POW_TAB_BIAS) / 16.0) as f32;
        }

        let block_max = frame_len;

        Ok(Core {
            version,
            channels,
            use_exp_vlc,
            use_bit_reservoir,
            use_variable_block_len,
            use_noise_coding,
            byte_offset_bits,
            exp_codebook,
            hgain_codebook,
            coef_tables,
            exponent_sizes,
            exponent_bands,
            high_band_start,
            coefs_start,
            coefs_end,
            exponent_high_sizes,
            exponent_high_bands,
            high_band_coded: [[false; MAX_BANDS]; MAX_CHANNELS],
            high_band_values: [[0; MAX_BANDS]; MAX_CHANNELS],
            frame_len,
            frame_len_bits,
            nb_block_sizes,
            reset_block_lengths: true,
            block_len_bits: frame_len_bits,
            next_block_len_bits: frame_len_bits,
            prev_block_len_bits: frame_len_bits,
            block_len: 0,
            block_pos: 0,
            ms_stereo: false,
            channel_coded: [false; MAX_CHANNELS],
            exponents_bsize: [0; MAX_CHANNELS],
            exponents: [vec![0.0; block_max], vec![0.0; block_max]],
            max_exponent: [1.0; MAX_CHANNELS],
            coefs1: [vec![0.0; block_max], vec![0.0; block_max]],
            coefs: [vec![0.0; block_max], vec![0.0; block_max]],
            output: vec![0.0; 2 * block_max],
            imdct,
            windows,
            frame_out: [vec![0.0; 2 * block_max], vec![0.0; 2 * block_max]],
            exponents_initialized: [false; MAX_CHANNELS],
            noise_table,
            noise_index: 0,
            noise_mult,
            lsp_cos_table,
            lsp_pow_e_table,
            lsp_pow_m_table1,
            lsp_pow_m_table2,
            pow_tab,
        })
    }

    /// Forgets the overlap with the previous frame (the part of `flush` that is not about the
    /// bit reservoir; FFmpeg keeps the stale overlap, which only affects the first frame after a
    /// seek).
    pub fn reset(&mut self) {
        for out in &mut self.frame_out {
            out.fill(0.0);
        }
    }

    /// `x^-0.25` with an exponent and mantissa table and linear interpolation (`pow_m1_4`).
    fn pow_m1_4(&self, x: f32) -> f32 {
        let u = x.to_bits();
        let e = (u >> 23) as usize;
        let m = ((u >> (23 - LSP_POW_BITS)) & ((1 << LSP_POW_BITS) - 1)) as usize;
        // Build the interpolation scale: 1 <= t < 2.
        let t = f32::from_bits(((u << LSP_POW_BITS) & ((1 << 23) - 1)) | (127 << 23));
        let a = self.lsp_pow_m_table1[m];
        let b = self.lsp_pow_m_table2[m];
        // A negative or non-finite value is not produced by a valid LSP curve.
        self.lsp_pow_e_table.get(e).copied().unwrap_or(0.0) * (a + b * t)
    }

    /// Decodes the exponents coded with LSP coefficients, the same idea as in Vorbis
    /// (`decode_exp_lsp` and `wma_lsp_to_curve`).
    fn decode_exp_lsp(&mut self, gb: &mut Bits<'_>, ch: usize) -> Result<()> {
        let mut lsp = [0f32; NB_LSP_COEFS];

        for (i, coef) in lsp.iter_mut().enumerate() {
            let val = if i == 0 || i >= 8 { gb.read(3)? } else { gb.read(4)? };
            *coef = LSP_CODEBOOK[i][val as usize];
        }

        let n = self.block_len;
        let mut val_max = 0f32;

        for i in 0..n {
            let mut p = 0.5f32;
            let mut q = 0.5f32;
            let w = self.lsp_cos_table[i];
            let mut j = 1;
            while j < NB_LSP_COEFS {
                q *= w - lsp[j - 1];
                p *= w - lsp[j];
                j += 2;
            }
            // As in FFmpeg: this is `p^3 (2 - w)` and `q^3 (2 + w)`, not `p^2 (2 - w)`.
            #[allow(clippy::misrefactored_assign_op)]
            {
                p *= p * (2.0 - w);
                q *= q * (2.0 + w);
            }
            let v = self.pow_m1_4(p + q);
            if v > val_max {
                val_max = v;
            }
            self.exponents[ch][i] = v;
        }

        self.max_exponent[ch] = val_max;
        Ok(())
    }

    /// Decodes the exponents coded with VLC codes (`decode_exp_vlc`).
    fn decode_exp_vlc(&mut self, gb: &mut Bits<'_>, ch: usize) -> Result<()> {
        let Some(codebook) = self.exp_codebook.as_ref()
        else {
            return decode_error("wma: missing exponent codebook");
        };

        let bands = &self.exponent_bands[self.frame_len_bits - self.block_len_bits];
        let n_bands = self.exponent_sizes[self.frame_len_bits - self.block_len_bits];
        let exponents = &mut self.exponents[ch];
        let q_end = self.block_len;

        let mut q = 0;
        let mut band = 0;
        let mut max_scale = 0f32;
        let mut last_exp: i32;

        // Fills the next band with an exponent.
        let mut fill = |q: &mut usize, band: &mut usize, v: f32| -> Result<()> {
            if *band >= n_bands {
                return decode_error("wma: too many exponent codes");
            }
            let n = usize::from(bands[*band]);
            *band += 1;
            let end = (*q + n).min(q_end);
            exponents[*q..end].fill(v);
            *q = end;
            Ok(())
        };

        if self.version == 1 {
            last_exp = gb.read(5)? as i32 + 10;
            let v = self.pow_tab[(last_exp + POW_TAB_BIAS) as usize];
            max_scale = v;
            fill(&mut q, &mut band, v)?;
        }
        else {
            last_exp = 36;
        }

        while q < q_end {
            let code = i32::from(gb.codebook(codebook)?);
            // This offset is the same as in MPEG-4 AAC.
            last_exp += code - 60;
            let idx = last_exp + POW_TAB_BIAS;
            if idx < 0 || idx as usize >= POW_TAB_LEN {
                return decode_error("wma: exponent out of range");
            }
            let v = self.pow_tab[idx as usize];
            if v > max_scale {
                max_scale = v;
            }
            fill(&mut q, &mut band, v)?;
        }

        self.max_exponent[ch] = max_scale;
        Ok(())
    }

    /// Applies the MDCT window and adds the block into the output (`wma_window`).
    ///
    /// We ensure that when the windows overlap their squared sum is always 1 (MDCT reconstruction
    /// rule).
    fn window(&mut self, ch: usize, out_start: usize) {
        let block_len = self.block_len;
        let input = &self.output;
        let out = &mut self.frame_out[ch][out_start..];

        // The left part.
        if self.block_len_bits <= self.prev_block_len_bits {
            let bsize = self.frame_len_bits - self.block_len_bits;
            let win = &self.windows[bsize];
            for i in 0..block_len {
                out[i] += input[i] * win[i];
            }
        }
        else {
            let prev_len = 1usize << self.prev_block_len_bits;
            let n = (block_len - prev_len) / 2;
            let bsize = self.frame_len_bits - self.prev_block_len_bits;
            let win = &self.windows[bsize];
            for i in 0..prev_len {
                out[n + i] += input[n + i] * win[i];
            }
            out[n + prev_len..n + prev_len + n].copy_from_slice(&input[n + prev_len..][..n]);
        }

        let out = &mut out[block_len..];
        let input = &input[block_len..];

        // The right part.
        if self.block_len_bits <= self.next_block_len_bits {
            let bsize = self.frame_len_bits - self.block_len_bits;
            let win = &self.windows[bsize];
            for i in 0..block_len {
                out[i] = input[i] * win[block_len - 1 - i];
            }
        }
        else {
            let next_len = 1usize << self.next_block_len_bits;
            let n = (block_len - next_len) / 2;
            let bsize = self.frame_len_bits - self.next_block_len_bits;
            let win = &self.windows[bsize];

            out[..n].copy_from_slice(&input[..n]);
            for i in 0..next_len {
                out[n + i] = input[n + i] * win[next_len - 1 - i];
            }
            out[n + next_len..n + next_len + n].fill(0.0);
        }
    }

    /// Decodes a block (`wma_decode_block`). Returns true if the block is the last of the frame.
    fn decode_block(&mut self, gb: &mut Bits<'_>) -> Result<bool> {
        let channels = self.channels;

        // Compute the current block length.
        if self.use_variable_block_len {
            let n = log2(self.nb_block_sizes as u32 - 1) + 1;

            if self.reset_block_lengths {
                self.reset_block_lengths = false;
                let v = gb.read(n)? as usize;
                if v >= self.nb_block_sizes {
                    return decode_error("wma: previous block length out of range");
                }
                self.prev_block_len_bits = self.frame_len_bits - v;
                let v = gb.read(n)? as usize;
                if v >= self.nb_block_sizes {
                    return decode_error("wma: block length out of range");
                }
                self.block_len_bits = self.frame_len_bits - v;
            }
            else {
                // Update the block lengths.
                self.prev_block_len_bits = self.block_len_bits;
                self.block_len_bits = self.next_block_len_bits;
            }
            let v = gb.read(n)? as usize;
            if v >= self.nb_block_sizes {
                return decode_error("wma: next block length out of range");
            }
            self.next_block_len_bits = self.frame_len_bits - v;
        }
        else {
            // A fixed block length.
            self.next_block_len_bits = self.frame_len_bits;
            self.prev_block_len_bits = self.frame_len_bits;
            self.block_len_bits = self.frame_len_bits;
        }

        if self.frame_len_bits - self.block_len_bits >= self.nb_block_sizes {
            return decode_error("wma: invalid block length");
        }

        // Check that the block length is coherent with the frame length.
        self.block_len = 1 << self.block_len_bits;
        if self.block_pos + self.block_len > self.frame_len {
            return decode_error("wma: frame length overflow");
        }

        if channels == 2 {
            self.ms_stereo = gb.bit()?;
        }

        let mut any_coded = false;
        for ch in 0..channels {
            let a = gb.bit()?;
            self.channel_coded[ch] = a;
            any_coded |= a;
        }

        let bsize = self.frame_len_bits - self.block_len_bits;

        // If no channel is coded, no need to go further.
        if any_coded {
            self.decode_coded_block(gb, bsize)?;
        }

        // The IMDCT, the window, and the overlap-add.
        for ch in 0..channels {
            let n4 = self.block_len / 2;

            if self.channel_coded[ch] {
                let n = self.block_len;
                self.imdct[bsize].imdct(&self.coefs[ch][..n], &mut self.output[..2 * n]);
            }
            else if !(self.ms_stereo && ch == 1) {
                self.output.fill(0.0);
            }

            // Multiply by the window and add in the frame.
            let index = (self.frame_len / 2) + self.block_pos - n4;
            self.window(ch, index);
        }

        // Update the block number.
        self.block_pos += self.block_len;
        Ok(self.block_pos >= self.frame_len)
    }

    /// The part of `wma_decode_block` that decodes the coefficients of the coded channels.
    fn decode_coded_block(&mut self, gb: &mut Bits<'_>, bsize: usize) -> Result<()> {
        let channels = self.channels;
        let block_len = self.block_len;

        // Read the total gain and extract the corresponding number of bits for the coefficient
        // escape coding.
        let mut total_gain = 1i32;
        loop {
            if gb.left() < 7 {
                return decode_error("wma: total gain overread");
            }
            let a = gb.read(7)? as i32;
            total_gain += a;
            if a != 127 {
                break;
            }
        }

        let coef_nb_bits = total_gain_to_bits(total_gain);

        // Compute the number of coefficients.
        let n = self.coefs_end[bsize] - self.coefs_start;
        let mut nb_coefs = [n; MAX_CHANNELS];

        // The complex coding.
        if self.use_noise_coding {
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    for i in 0..self.exponent_high_sizes[bsize] {
                        let a = gb.bit()?;
                        self.high_band_coded[ch][i] = a;
                        // With noise coding, the coefficients are not transmitted.
                        if a {
                            nb_coefs[ch] =
                                nb_coefs[ch].saturating_sub(self.exponent_high_bands[bsize][i]);
                        }
                    }
                }
            }

            let Some(hgain) = self.hgain_codebook.as_ref()
            else {
                return decode_error("wma: missing high band gain codebook");
            };

            for ch in 0..channels {
                if self.channel_coded[ch] {
                    let mut val: Option<i32> = None;
                    for i in 0..self.exponent_high_sizes[bsize] {
                        if self.high_band_coded[ch][i] {
                            let v = match val {
                                None => gb.read(7)? as i32 - 19,
                                Some(v) => v + i32::from(gb.codebook(hgain)?) - 18,
                            };
                            val = Some(v);
                            self.high_band_values[ch][i] = v;
                        }
                    }
                }
            }
        }

        // The exponents can be reused in short blocks.
        if self.block_len_bits == self.frame_len_bits || gb.bit()? {
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    if self.use_exp_vlc {
                        self.decode_exp_vlc(gb, ch)?;
                    }
                    else {
                        self.decode_exp_lsp(gb, ch)?;
                    }
                    self.exponents_bsize[ch] = bsize;
                    self.exponents_initialized[ch] = true;
                }
            }
        }

        for ch in 0..channels {
            if self.channel_coded[ch] && !self.exponents_initialized[ch] {
                return decode_error("wma: exponents are not initialised");
            }
        }

        // Parse the spectral coefficients: just RLE encoding.
        for ch in 0..channels {
            if self.channel_coded[ch] {
                // Special VLC tables are used for MS stereo because there is potentially less
                // energy there.
                let tindex = usize::from(ch == 1 && self.ms_stereo);
                self.coefs1[ch][..block_len].fill(0.0);
                self.run_level_decode(gb, ch, tindex, nb_coefs[ch], coef_nb_bits)?;
            }
            if self.version == 1 && channels >= 2 {
                gb.align()?;
            }
        }

        // Normalise.
        let n4 = block_len / 2;
        let mut mdct_norm = (1.0 / n4 as f64) as f32;
        if self.version == 1 {
            mdct_norm = (f64::from(mdct_norm) * (n4 as f64).sqrt()) as f32;
        }

        // Finally compute the MDCT coefficients.
        for ch in 0..channels {
            if self.channel_coded[ch] {
                self.compute_coefs(ch, bsize, total_gain, mdct_norm, nb_coefs[ch])?;
            }
        }

        if self.ms_stereo && self.channel_coded[1] {
            // The nominal case for MS stereo: it is done before the MDCT.
            if !self.channel_coded[0] {
                // The rare MS stereo case.
                self.coefs[0][..block_len].fill(0.0);
                self.channel_coded[0] = true;
            }

            let (c0, c1) = self.coefs.split_at_mut(1);
            for (a, b) in c0[0][..block_len].iter_mut().zip(&mut c1[0][..block_len]) {
                let t = *a - *b;
                *a += *b;
                *b = t;
            }
        }

        Ok(())
    }

    /// Decodes the run level compressed coefficients (`ff_wma_run_level_decode` with version 0).
    fn run_level_decode(
        &mut self,
        gb: &mut Bits<'_>,
        ch: usize,
        tindex: usize,
        num_coefs: usize,
        coef_nb_bits: u32,
    ) -> Result<()> {
        let table = &self.coef_tables[tindex];
        let ptr = &mut self.coefs1[ch];
        let coef_mask = self.block_len - 1;
        let frame_len_bits = self.frame_len_bits as u32;

        let mut offset = 0usize;

        while offset < num_coefs {
            let code = usize::from(gb.codebook(&table.codebook)?);

            if code > 1 {
                // A normal code.
                offset += usize::from(table.run[code]);
                // A set bit is a positive level (`sign = get_bits1() - 1` in FFmpeg).
                let negative = !gb.bit()?;
                let level = table.level[code];
                ptr[offset & coef_mask] = if negative { -level } else { level };
            }
            else if code == 1 {
                // The end of the block.
                break;
            }
            else {
                // An escape. Reading `block_len_bits` would be better than `frame_len_bits`, but
                // it is how the streams are coded.
                let level = gb.read(coef_nb_bits)?;
                offset += gb.read(frame_len_bits)? as usize;
                let negative = !gb.bit()?;
                let level = level as f32;
                ptr[offset & coef_mask] = if negative { -level } else { level };
            }

            offset += 1;
        }

        // The end of the block can be omitted.
        if offset > num_coefs {
            return decode_error("wma: overflow in spectral RLE");
        }

        Ok(())
    }

    /// Computes the MDCT coefficients of a channel from the decoded levels and exponents, and adds
    /// the noise (the last part of the loop of `wma_decode_block`).
    fn compute_coefs(
        &mut self,
        ch: usize,
        bsize: usize,
        total_gain: i32,
        mdct_norm: f32,
        nb_coefs: usize,
    ) -> Result<()> {
        let block_len = self.block_len;
        let esize = self.exponents_bsize[ch];
        let exponents = &self.exponents[ch];
        let coefs1 = &self.coefs1[ch];
        let coefs = &mut self.coefs[ch];

        // The index of the exponent of the coefficient `i` of the current block.
        let exp_index = |i: usize| (i << bsize) >> esize;

        let mut mult =
            (10f64.powf(f64::from(total_gain) * 0.05) / f64::from(self.max_exponent[ch])) as f32;
        mult *= mdct_norm;

        let mut c = 0; // The index in `coefs`.

        if self.use_noise_coding {
            let noise_table = &self.noise_table;
            let mut noise_index = self.noise_index;
            let mut next_noise = || {
                let v = noise_table[noise_index];
                noise_index = (noise_index + 1) & (NOISE_TAB_SIZE - 1);
                v
            };

            let mult1 = mult;
            // Very low frequencies: noise.
            for i in 0..self.coefs_start {
                coefs[c] = next_noise() * exponents[exp_index(i)] * mult1;
                c += 1;
            }

            let n1 = self.exponent_high_sizes[bsize];

            // Compute the power of the high bands.
            let mut exp_pos = exp_index(self.high_band_start[bsize]);
            let mut exp_power = [0f32; MAX_BANDS];
            let mut last_high_band = 0;
            for j in 0..n1 {
                let n = self.exponent_high_bands[bsize][j];
                if self.high_band_coded[ch][j] {
                    let mut e2 = 0f32;
                    for i in 0..n {
                        let v = exponents[exp_pos + exp_index(i)];
                        e2 += v * v;
                    }
                    exp_power[j] = e2 / n as f32;
                    last_high_band = j;
                }
                exp_pos += (n << bsize) >> esize;
            }

            // The main and the high frequencies.
            let mut exp_pos = exp_index(self.coefs_start);
            let mut c1 = 0; // The index in `coefs1`.
            for j in -1..n1 as isize {
                let n = if j < 0 {
                    self.high_band_start[bsize].saturating_sub(self.coefs_start)
                }
                else {
                    self.exponent_high_bands[bsize][j as usize]
                };

                if j >= 0 && self.high_band_coded[ch][j as usize] {
                    let j = j as usize;
                    // Use noise with the specified power.
                    let mut mult1 = (exp_power[j] / exp_power[last_high_band]).sqrt();
                    mult1 = (f64::from(mult1)
                        * 10f64.powf(f64::from(self.high_band_values[ch][j]) * 0.05))
                        as f32;
                    mult1 /= self.max_exponent[ch] * self.noise_mult;
                    mult1 *= mdct_norm;
                    for i in 0..n {
                        let noise = next_noise();
                        coefs[c] = noise * exponents[exp_pos + exp_index(i)] * mult1;
                        c += 1;
                    }
                }
                else {
                    // The coded values and small noise.
                    for i in 0..n {
                        let noise = next_noise();
                        coefs[c] = (coefs1[c1] + noise) * exponents[exp_pos + exp_index(i)] * mult;
                        c += 1;
                        c1 += 1;
                    }
                }
                exp_pos += (n << bsize) >> esize;
            }

            // Very high frequencies: noise.
            let n = block_len - self.coefs_end[bsize];
            let last = exp_pos as isize + ((-(1isize << bsize)) >> esize);
            let Some(&last_exp) = usize::try_from(last).ok().and_then(|p| exponents.get(p))
            else {
                return decode_error("wma: invalid exponent index");
            };
            let mult1 = mult * last_exp;
            for _ in 0..n {
                coefs[c] = next_noise() * mult1;
                c += 1;
            }

            self.noise_index = noise_index;
        }
        else {
            for _ in 0..self.coefs_start {
                coefs[c] = 0.0;
                c += 1;
            }
            for i in 0..nb_coefs {
                coefs[c] = coefs1[i] * exponents[exp_index(i)] * mult;
                c += 1;
            }
            for _ in 0..block_len - self.coefs_end[bsize] {
                coefs[c] = 0.0;
                c += 1;
            }
        }

        Ok(())
    }

    /// Decodes a frame of `frame_len` samples (`wma_decode_frame`, without the output copy; see
    /// [`Core::take_frame`]).
    pub fn decode_frame(&mut self, gb: &mut Bits<'_>) -> Result<()> {
        // Read each block.
        self.block_pos = 0;
        while !self.decode_block(gb)? {}
        Ok(())
    }

    /// Moves the decoded frame out of the overlap-add buffer, and prepares for the next frame.
    pub fn take_frame(&mut self, ch: usize, dst: &mut [f32]) {
        let fl = self.frame_len;
        let out = &mut self.frame_out[ch];
        dst[..fl].copy_from_slice(&out[..fl]);
        out.copy_within(fl..2 * fl, 0);
    }
}
