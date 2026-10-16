// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC3 frame decoder. Port of FFmpeg's `libavcodec/atrac3.c` (LGPL-2.1+, see `NOTICE`).
//!
//! A frame (`block_align` bytes) holds one sound unit per channel and decodes to 1024 samples per
//! channel. A sound unit codes four 256-sample QMF bands with a 256-point MDCT each, with optional
//! tonal components and gain control. Channels are coded either independently or, in joint stereo
//! mode, as pairs.

use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::vlc::{BitOrder, Codebook, CodebookBuilder, Entry8x8};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use crate::common::{
    GainCompensation, GainInfo, QMF_DELAY, bits, iqmf_finish, iqmf_prepare, make_qmf_window,
    make_sf_table,
};
use crate::tables::{
    CLC_LENGTH_TAB, HUFF_TAB_SIZES, HUFFTABS, INV_MAX_QUANT, MANTISSA_CLC_TAB, MANTISSA_VLC_TAB,
    MATRIX_COEFFS, SUBBAND_TAB,
};

/// Maximum number of channels.
pub(crate) const MAX_CHANNELS: usize = 8;
/// Maximum number of joint stereo channel pairs coded with their own matrixing state.
const MAX_JS_PAIRS: usize = 4;

/// Number of samples per channel in a frame.
pub(crate) const SAMPLES_PER_FRAME: usize = 1024;
/// Number of samples of the transform of one QMF band.
const MDCT_SIZE: usize = 512;
/// Largest supported block align (`atrac3_decode_init`).
const MAX_BLOCK_ALIGN: usize = 4096;

/// Value of the coding mode of joint stereo streams.
const JOINT_STEREO: u16 = 0x12;
/// Value of the coding mode of streams with independently coded channels.
const SINGLE: u16 = 0x2;

/// Inverse MDCT scale: `1 / 2^15`.
const IMDCT_SCALE: f64 = 1.0 / 32768.0;

/// Key used to descramble the data of RealMedia streams (`decode_bytes`).
const DESCRAMBLE_KEY: [u8; 4] = [0x53, 0x7f, 0x61, 0x03];

/// Maximum number of tonal components in a sound unit.
const MAX_COMPONENTS: usize = 64;

/// Layout of the codec extra data.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// The 14 bytes of a WAVE `fmt ` chunk extension: each frame holds `block_align` bytes,
    /// `block_align` must be `{96, 152, 192} * channels * frame_factor`.
    Wave { frame_factor: u16 },
    /// The 10 or 12 bytes of a RealAudio `atrc` stream: the frame data is scrambled.
    Real,
}

/// Parameters from the codec extra data.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Atrac3Config {
    layout: Layout,
    coding_mode: u16,
}

impl Atrac3Config {
    /// Parses the extra data (`atrac3_decode_init`).
    pub fn parse(extra: &[u8], channels: usize) -> Result<Self> {
        let le16 = |o: usize| u16::from_le_bytes([extra[o], extra[o + 1]]);
        let be16 = |o: usize| u16::from_be_bytes([extra[o], extra[o + 1]]);

        let (layout, version, samples_per_frame, delay, coding_mode) = match extra.len() {
            14 => {
                // WAVE: [0..2] unknown (always 1), [2..6] samples per channel, [6..8] coding mode,
                // [8..10] copy of the coding mode, [10..12] frame factor, [12..14] unknown.
                let mode = if le16(6) != 0 { JOINT_STEREO } else { SINGLE };
                (
                    Layout::Wave { frame_factor: le16(10) },
                    4,
                    (SAMPLES_PER_FRAME * channels) as u32,
                    0x88e,
                    mode,
                )
            }
            10 | 12 => {
                // RealAudio: version (32 bits), samples per frame, delay, coding mode.
                let version = u32::from_be_bytes([extra[0], extra[1], extra[2], extra[3]]);
                (Layout::Real, version, u32::from(be16(4)), be16(6), be16(8))
            }
            _ => return unsupported_error("atrac3: unknown extra data size"),
        };

        if version != 4 {
            return unsupported_error("atrac3: unsupported version");
        }

        if samples_per_frame != (SAMPLES_PER_FRAME * channels) as u32 {
            return unsupported_error("atrac3: unknown amount of samples per frame");
        }

        if delay != 0x88e {
            return unsupported_error("atrac3: unknown amount of delay");
        }

        match coding_mode {
            SINGLE => (),
            JOINT_STEREO => {
                // Joint stereo channels must be even.
                if channels % 2 == 1 {
                    return decode_error("atrac3: invalid joint stereo channel configuration");
                }
            }
            _ => return unsupported_error("atrac3: unknown channel coding mode"),
        }

        Ok(Atrac3Config { layout, coding_mode })
    }
}

/// A tonal component: a few spectral lines coded separately.
#[derive(Copy, Clone, Default)]
struct TonalComponent {
    pos: usize,
    num_coefs: usize,
    coef: [f32; 8],
}

/// The state of one channel (`ChannelUnit`).
struct ChannelUnit {
    /// Number of QMF bands coded, minus 1.
    bands_coded: usize,
    num_components: usize,
    /// The overlap of the previous frame.
    prev_frame: [f32; SAMPLES_PER_FRAME],
    /// Index of the gain block that holds the gain control data of the current frame.
    gc_blk_switch: usize,
    components: [TonalComponent; MAX_COMPONENTS],
    /// Gain control of the previous and the next frame, of each band.
    gain_block: [[GainInfo; 4]; 2],
    spectrum: [f32; SAMPLES_PER_FRAME],
    /// QMF delay buffers.
    delay_buf1: [f32; QMF_DELAY],
    delay_buf2: [f32; QMF_DELAY],
    delay_buf3: [f32; QMF_DELAY],
}

impl ChannelUnit {
    fn new() -> Box<Self> {
        Box::new(ChannelUnit {
            bands_coded: 0,
            num_components: 0,
            prev_frame: [0.0; SAMPLES_PER_FRAME],
            gc_blk_switch: 0,
            components: [TonalComponent::default(); MAX_COMPONENTS],
            gain_block: [[GainInfo::default(); 4]; 2],
            spectrum: [0.0; SAMPLES_PER_FRAME],
            delay_buf1: [0.0; QMF_DELAY],
            delay_buf2: [0.0; QMF_DELAY],
            delay_buf3: [0.0; QMF_DELAY],
        })
    }
}

/// Joint stereo state of a channel pair.
#[derive(Copy, Clone)]
struct JointStereo {
    matrix_coeff_index_prev: [u8; 4],
    matrix_coeff_index_now: [u8; 4],
    matrix_coeff_index_next: [u8; 4],
    weighting_delay: [u8; 6],
}

impl JointStereo {
    fn new() -> Self {
        JointStereo {
            matrix_coeff_index_prev: [3; 4],
            matrix_coeff_index_now: [3; 4],
            matrix_coeff_index_next: [3; 4],
            weighting_delay: [0, 7, 0, 7, 0, 7],
        }
    }
}

/// Working buffers and tables shared by all channels.
struct Dsp {
    imdct: Imdct,
    /// Output of the IMDCT of one band.
    imdct_buf: [f32; MDCT_SIZE],
    /// The IMDCT window.
    mdct_window: [f32; MDCT_SIZE],
    temp_buf: [f32; QMF_DELAY + SAMPLES_PER_FRAME],
    gainc: GainCompensation,
    sf_table: [f32; 64],
    qmf_window: [f32; 48],
    /// Spectral coefficient codebooks, for selectors `1..=7`.
    codebooks: Vec<Codebook<Entry8x8>>,
}

/// The ATRAC3 frame decoder (`ATRAC3Context`).
pub(crate) struct Atrac3Core {
    config: Atrac3Config,
    channels: usize,
    units: Vec<Box<ChannelUnit>>,
    js: [JointStereo; MAX_JS_PAIRS],
    dsp: Box<Dsp>,
    /// Descrambled copy of a frame.
    descrambled: Vec<u8>,
    /// Reversed copy of the data of a joint stereo pair.
    reversed: Vec<u8>,
}

/// Generates the IMDCT window (<http://wiki.multimedia.cx/index.php?title=RealAudio_atrc#Windows>).
fn make_imdct_window() -> [f32; MDCT_SIZE] {
    let mut w = [0.0f32; MDCT_SIZE];

    let (mut i, mut j) = (0usize, 255usize);
    while i < 128 {
        let wi = (((i as f64 + 0.5) / 256.0 - 0.5) * std::f64::consts::PI).sin() + 1.0;
        let wj = (((j as f64 + 0.5) / 256.0 - 0.5) * std::f64::consts::PI).sin() + 1.0;
        let ww = 0.5 * (wi * wi + wj * wj);

        w[i] = (wi / ww) as f32;
        w[511 - i] = w[i];
        w[j] = (wj / ww) as f32;
        w[511 - j] = w[j];

        i += 1;
        j -= 1;
    }

    w
}

/// Builds the spectral coefficient codebooks. Codes are assigned in table order, increasing from
/// zero, as in `ff_vlc_init_from_lengths`; the symbols are stored with an offset of 31.
fn make_codebooks() -> Vec<Codebook<Entry8x8>> {
    let mut codebooks = Vec::with_capacity(HUFF_TAB_SIZES.len());
    let mut start = 0;

    for &size in &HUFF_TAB_SIZES {
        let table = &HUFFTABS[start..start + usize::from(size)];
        start += usize::from(size);

        let mut codes = Vec::with_capacity(table.len());
        let mut lens = Vec::with_capacity(table.len());
        let mut values = Vec::with_capacity(table.len());

        let mut code = 0u64;

        for &(sym, len) in table {
            // Codes are left-aligned in 32 bits in the reference; make them right-aligned.
            codes.push((code >> (32 - u32::from(len))) as u32);
            lens.push(len);
            values.push(sym);
            code += 1u64 << (32 - u32::from(len));
        }

        let mut builder = CodebookBuilder::new(BitOrder::Verbatim);
        builder.bits_per_read(8);

        codebooks.push(builder.make(&codes, &lens, &values).expect("valid static codebook data"));
    }

    codebooks
}

impl Atrac3Core {
    pub fn new(config: Atrac3Config, channels: usize) -> Result<Self> {
        if channels < 1 || channels > MAX_CHANNELS {
            return unsupported_error("atrac3: unsupported number of channels");
        }

        let dsp = Box::new(Dsp {
            imdct: Imdct::new_scaled(256, IMDCT_SCALE),
            imdct_buf: [0.0; MDCT_SIZE],
            mdct_window: make_imdct_window(),
            temp_buf: [0.0; QMF_DELAY + SAMPLES_PER_FRAME],
            gainc: GainCompensation::new(4, 3),
            sf_table: make_sf_table(),
            qmf_window: make_qmf_window(),
            codebooks: make_codebooks(),
        });

        Ok(Atrac3Core {
            config,
            channels,
            units: (0..channels).map(|_| ChannelUnit::new()).collect(),
            js: [JointStereo::new(); MAX_JS_PAIRS],
            dsp,
            descrambled: vec![0; MAX_BLOCK_ALIGN],
            reversed: vec![0; MAX_BLOCK_ALIGN],
        })
    }

    pub fn reset(&mut self) {
        for u in &mut self.units {
            *u = ChannelUnit::new();
        }
        self.js = [JointStereo::new(); MAX_JS_PAIRS];
    }

    /// Validates the size of a frame, which is that of the packet.
    pub fn check_block_align(&self, block_align: usize) -> Result<()> {
        if block_align == 0 || block_align > MAX_BLOCK_ALIGN {
            return decode_error("atrac3: invalid block align");
        }

        if let Layout::Wave { frame_factor } = self.config.layout {
            let unit = self.channels * usize::from(frame_factor);
            if block_align != 96 * unit && block_align != 152 * unit && block_align != 192 * unit {
                return decode_error("atrac3: unknown frame/channel/frame_factor configuration");
            }
        }

        Ok(())
    }

    /// Decodes one frame of `data.len()` bytes into `out[ch][..1024]` (`atrac3_decode_frame`).
    pub fn decode_frame(&mut self, data: &[u8], out: &mut [Vec<f32>]) -> Result<()> {
        let block_align = data.len();

        self.check_block_align(block_align)?;

        // Descramble the data of RealMedia streams.
        let data = if self.config.layout == Layout::Real {
            for (i, (o, &b)) in self.descrambled.iter_mut().zip(data).enumerate() {
                *o = b ^ DESCRAMBLE_KEY[i & 3];
            }
            &self.descrambled[..block_align]
        }
        else {
            data
        };

        let channels = self.channels;

        if self.config.coding_mode == JOINT_STEREO {
            // Channel coupling mode: decode pairs of sound units. Multichannel joint stereo
            // interleaves the pairs (6 channels: 2 + 2 + 2).
            let js_block_align = (block_align / channels) * 2;

            for ch in (0..channels).step_by(2) {
                let pair = ch / 2;

                let Some(js_data) = data.get(pair * js_block_align..(pair + 1) * js_block_align)
                else {
                    return decode_error("atrac3: frame too small");
                };

                // Decode the first sound unit.
                let (out1, out2) = out[ch..].split_at_mut(1);
                let (out1, out2) = (&mut out1[0][..], &mut out2[0][..]);

                let mut br = BitReaderLtr::new(js_data);
                self.dsp.decode_channel_sound_unit(
                    &mut br,
                    &mut self.units[ch],
                    out1,
                    ch,
                    JOINT_STEREO,
                )?;

                // The data of the second sound unit is encoded in reverse byte order.
                let reversed = &mut self.reversed[..js_block_align];
                for (r, &b) in reversed.iter_mut().zip(js_data.iter().rev()) {
                    *r = b;
                }

                // Skip the sync codes (0xf8).
                let mut start = 0;
                let mut i = 4;
                while reversed.get(start) == Some(&0xf8) {
                    if i >= js_block_align {
                        return decode_error("atrac3: invalid joint stereo sync codes");
                    }
                    start += 1;
                    i += 1;
                }

                // Start reading the second sound unit.
                let mut br = BitReaderLtr::new(&reversed[start..]);

                let js = &mut self.js[pair];

                // Fill the weighting coefficients delay buffer.
                js.weighting_delay.copy_within(2.., 0);
                js.weighting_delay[4] = bits(br.read_bits_leq32(1))? as u8;
                js.weighting_delay[5] = bits(br.read_bits_leq32(3))? as u8;

                for i in 0..4 {
                    js.matrix_coeff_index_prev[i] = js.matrix_coeff_index_now[i];
                    js.matrix_coeff_index_now[i] = js.matrix_coeff_index_next[i];
                    js.matrix_coeff_index_next[i] = bits(br.read_bits_leq32(2))? as u8;
                }

                // Decode the second sound unit.
                self.dsp.decode_channel_sound_unit(
                    &mut br,
                    &mut self.units[ch + 1],
                    out2,
                    ch + 1,
                    JOINT_STEREO,
                )?;

                // Reconstruct the channel coefficients.
                reverse_matrixing(
                    out1,
                    out2,
                    &js.matrix_coeff_index_prev,
                    &js.matrix_coeff_index_now,
                );

                channel_weighting(out1, out2, &js.weighting_delay);
            }
        }
        else {
            // Independently coded channels: decode the sound units.
            let unit_len = block_align / channels;

            for (i, (unit, out)) in self.units.iter_mut().zip(out.iter_mut()).enumerate() {
                let Some(unit_data) = data.get(i * unit_len..(i + 1) * unit_len)
                else {
                    return decode_error("atrac3: frame too small");
                };

                let mut br = BitReaderLtr::new(unit_data);
                self.dsp.decode_channel_sound_unit(&mut br, unit, out, i, SINGLE)?;
            }
        }

        // Apply the iQMF synthesis filter.
        for (unit, out) in self.units.iter_mut().zip(out.iter_mut()) {
            self.dsp.synthesis(unit, out);
        }

        Ok(())
    }
}

/// Reads the quantized spectral coefficients of a subband (`read_quant_spectral_coeffs`).
///
/// `selector` selects the table the values are coded with, `coding_flag` is the constant length
/// coding (CLC) flag, otherwise variable length coding (VLC) is used. `num_codes` values are read
/// into `mantissas`.
fn read_quant_spectral_coeffs(
    br: &mut BitReaderLtr<'_>,
    codebooks: &[Codebook<Entry8x8>],
    selector: usize,
    coding_flag: bool,
    mantissas: &mut [i32],
    num_codes: usize,
) -> Result<()> {
    let num_codes = if selector == 1 { num_codes / 2 } else { num_codes };

    if coding_flag {
        // Constant length coding.
        let num_bits = u32::from(CLC_LENGTH_TAB[selector]);

        if selector > 1 {
            for m in mantissas.iter_mut().take(num_codes) {
                *m = if num_bits != 0 { bits(br.read_bits_leq32_signed(num_bits))? } else { 0 };
            }
        }
        else {
            for pair in mantissas.chunks_exact_mut(2).take(num_codes) {
                // `num_bits` is always 4 in this case.
                let code =
                    if num_bits != 0 { bits(br.read_bits_leq32(num_bits))? as usize } else { 0 };
                pair[0] = i32::from(MANTISSA_CLC_TAB[code >> 2]);
                pair[1] = i32::from(MANTISSA_CLC_TAB[code & 3]);
            }
        }
    }
    else {
        // Variable length coding.
        let codebook = &codebooks[selector - 1];

        if selector != 1 {
            for m in mantissas.iter_mut().take(num_codes) {
                let (sym, _) = bits(br.read_codebook(codebook))?;
                *m = i32::from(sym) - 31;
            }
        }
        else {
            for pair in mantissas.chunks_exact_mut(2).take(num_codes) {
                let (sym, _) = bits(br.read_codebook(codebook))?;
                let sym = usize::from(sym) - 31;
                pair[0] = i32::from(MANTISSA_VLC_TAB[sym * 2]);
                pair[1] = i32::from(MANTISSA_VLC_TAB[sym * 2 + 1]);
            }
        }
    }

    Ok(())
}

/// Decodes the gain parameters of the coded bands (`decode_gain_control`).
fn decode_gain_control(
    br: &mut BitReaderLtr<'_>,
    gain: &mut [GainInfo; 4],
    num_bands: usize,
) -> Result<()> {
    for g in gain.iter_mut().take(num_bands + 1) {
        g.num_points = bits(br.read_bits_leq32(3))? as usize;

        for j in 0..g.num_points {
            g.lev_code[j] = bits(br.read_bits_leq32(4))? as u8;
            g.loc_code[j] = bits(br.read_bits_leq32(5))? as u8;

            if j > 0 && g.loc_code[j] <= g.loc_code[j - 1] {
                return decode_error("atrac3: invalid gain control points");
            }
        }
    }

    // Clear the unused bands.
    for g in gain.iter_mut().skip(num_bands + 1) {
        g.num_points = 0;
    }

    Ok(())
}

/// Adds the tonal components to the spectrum (`add_tonal_components`). Returns the position after
/// the last tonal coefficient, or `None` if there are no components.
fn add_tonal_components(spectrum: &mut [f32], components: &[TonalComponent]) -> Option<usize> {
    let mut last_pos = None;

    for c in components {
        last_pos = last_pos.max(Some(c.pos + c.num_coefs));

        for (o, &i) in spectrum[c.pos..c.pos + c.num_coefs].iter_mut().zip(&c.coef) {
            *o += i;
        }
    }

    last_pos
}

/// `INTERPOLATE(old, new, nsample)`: linear interpolation over eight samples.
fn interpolate(old: f32, new: f32, nsample: usize) -> f64 {
    f64::from(old) + nsample as f64 * 0.125 * (f64::from(new) - f64::from(old))
}

/// Reverses the joint stereo matrixing of the first 4 * 256 samples (`reverse_matrixing`).
fn reverse_matrixing(su1: &mut [f32], su2: &mut [f32], prev_code: &[u8; 4], curr_code: &[u8; 4]) {
    for (i, band) in (0..4 * 256).step_by(256).enumerate() {
        let s1 = usize::from(prev_code[i]);
        let s2 = usize::from(curr_code[i]);
        let mut nsample = band;

        if s1 != s2 {
            // The selector value changed: interpolate.
            let mc1_l = MATRIX_COEFFS[s1 * 2];
            let mc1_r = MATRIX_COEFFS[s1 * 2 + 1];
            let mc2_l = MATRIX_COEFFS[s2 * 2];
            let mc2_r = MATRIX_COEFFS[s2 * 2 + 1];

            // The interpolation is done over the first eight samples.
            while nsample < band + 8 {
                let c1 = su1[nsample];
                let c2 = su2[nsample];
                let c2 = (f64::from(c1) * interpolate(mc1_l, mc2_l, nsample - band)
                    + f64::from(c2) * interpolate(mc1_r, mc2_r, nsample - band))
                    as f32;
                su1[nsample] = c2;
                su2[nsample] = (f64::from(c1) * 2.0 - f64::from(c2)) as f32;
                nsample += 1;
            }
        }

        // Apply the matrix without interpolation.
        let range = nsample..band + 256;

        match s2 {
            // M/S decoding.
            0 => {
                for (a, b) in su1[range.clone()].iter_mut().zip(su2[range].iter_mut()) {
                    let (c1, c2) = (*a, *b);
                    *a = c2 * 2.0;
                    *b = (c1 - c2) * 2.0;
                }
            }
            1 => {
                for (a, b) in su1[range.clone()].iter_mut().zip(su2[range].iter_mut()) {
                    let (c1, c2) = (*a, *b);
                    *a = (c1 + c2) * 2.0;
                    *b = c2 * -2.0;
                }
            }
            _ => {
                for (a, b) in su1[range.clone()].iter_mut().zip(su2[range].iter_mut()) {
                    let (c1, c2) = (*a, *b);
                    *a = c1 + c2;
                    *b = c1 - c2;
                }
            }
        }
    }
}

/// `get_channel_weights`.
fn get_channel_weights(index: u8, flag: u8) -> [f32; 2] {
    if index == 7 {
        [1.0, 1.0]
    }
    else {
        let w0 = (f64::from(index & 7) / 7.0) as f32;
        let w1 = f64::from(2.0f32 - w0 * w0).sqrt() as f32;

        if flag != 0 { [w1, w0] } else { [w0, w1] }
    }
}

/// `channel_weighting`.
fn channel_weighting(su1: &mut [f32], su2: &mut [f32], p3: &[u8; 6]) {
    if p3[1] != 7 || p3[3] != 7 {
        // `w[x][y]`: `y = 0` is left, `y = 1` is right.
        let w0 = get_channel_weights(p3[1], p3[0]);
        let w1 = get_channel_weights(p3[3], p3[2]);

        for band in (256..4 * 256).step_by(256) {
            for nsample in band..band + 8 {
                su1[nsample] =
                    (f64::from(su1[nsample]) * interpolate(w0[0], w0[1], nsample - band)) as f32;
                su2[nsample] =
                    (f64::from(su2[nsample]) * interpolate(w1[0], w1[1], nsample - band)) as f32;
            }

            for nsample in band + 8..band + 256 {
                su1[nsample] *= w1[0];
                su2[nsample] *= w1[1];
            }
        }
    }
}

impl Dsp {
    /// Regular 512 point IMDCT without overlapping, with the exception of the swapping of odd
    /// bands caused by the reverse spectra of the QMF (`imlt`).
    fn imlt(&mut self, input: &mut [f32], odd_band: bool) {
        if odd_band {
            // Reverse the odd bands before the IMDCT.
            input.reverse();
        }

        self.imdct.imdct(input, &mut self.imdct_buf);

        // Perform windowing on the output. FFmpeg's full IMDCT has the opposite sign convention to
        // Symphonia's, hence the negation.
        for (o, &w) in self.imdct_buf.iter_mut().zip(&self.mdct_window) {
            *o *= -w;
        }
    }

    /// Reads the quantized spectrum of a sound unit (`decode_spectrum`). Returns the number of
    /// subbands, minus 1.
    fn decode_spectrum(&self, br: &mut BitReaderLtr<'_>, output: &mut [f32]) -> Result<usize> {
        let mut subband_vlc_index = [0usize; 32];
        let mut sf_index = [0usize; 32];
        let mut mantissas = [0i32; 128];

        // Number of coded subbands, minus 1.
        let num_subbands = bits(br.read_bits_leq32(5))? as usize;
        // Coding mode: false is VLC, true is CLC.
        let coding_mode = bits(br.read_bool())?;

        // The VLC selector table of the subbands, 0 means not coded.
        for sel in subband_vlc_index.iter_mut().take(num_subbands + 1) {
            *sel = bits(br.read_bits_leq32(3))? as usize;
        }

        // The scale factor indexes.
        for (sf, &sel) in sf_index.iter_mut().zip(&subband_vlc_index).take(num_subbands + 1) {
            if sel != 0 {
                *sf = bits(br.read_bits_leq32(6))? as usize;
            }
        }

        for i in 0..=num_subbands {
            let first = usize::from(SUBBAND_TAB[i]);
            let last = usize::from(SUBBAND_TAB[i + 1]);
            let subband_size = last - first;
            let selector = subband_vlc_index[i];

            if selector != 0 {
                // Decode the spectral coefficients of this subband.
                read_quant_spectral_coeffs(
                    br,
                    &self.codebooks,
                    selector,
                    coding_mode,
                    &mut mantissas,
                    subband_size,
                )?;

                // Decode the scale factor of this subband.
                let scale_factor = self.sf_table[sf_index[i]] * INV_MAX_QUANT[selector];

                // Inverse quantize the coefficients.
                for (o, &m) in output[first..last].iter_mut().zip(&mantissas) {
                    *o = m as f32 * scale_factor;
                }
            }
            else {
                // This subband was not coded: zero it.
                output[first..last].fill(0.0);
            }
        }

        // Clear the subbands that were not coded.
        let first = usize::from(SUBBAND_TAB[num_subbands + 1]);
        output[first..].fill(0.0);

        Ok(num_subbands)
    }

    /// Restores the quantized tonal components (`decode_tonal_components`). Returns the number of
    /// components.
    fn decode_tonal_components(
        &self,
        br: &mut BitReaderLtr<'_>,
        components: &mut [TonalComponent; MAX_COMPONENTS],
        num_bands: usize,
    ) -> Result<usize> {
        let mut band_flags = [false; 4];
        let mut mantissa = [0i32; 8];
        let mut component_count = 0;

        let nb_components = bits(br.read_bits_leq32(5))? as usize;

        // No tonal components.
        if nb_components == 0 {
            return Ok(0);
        }

        let coding_mode_selector = bits(br.read_bits_leq32(2))?;
        if coding_mode_selector == 2 {
            return decode_error("atrac3: invalid tonal component coding mode");
        }

        let mut coding_mode = coding_mode_selector & 1 != 0;

        for _ in 0..nb_components {
            for flag in band_flags.iter_mut().take(num_bands + 1) {
                *flag = bits(br.read_bool())?;
            }

            let coded_values_per_component = bits(br.read_bits_leq32(3))? as usize;

            let quant_step_index = bits(br.read_bits_leq32(3))? as usize;
            if quant_step_index <= 1 {
                return decode_error("atrac3: invalid tonal component quantizer");
            }

            if coding_mode_selector == 3 {
                coding_mode = bits(br.read_bool())?;
            }

            for b in 0..(num_bands + 1) * 4 {
                if !band_flags[b >> 2] {
                    continue;
                }

                let coded_components = bits(br.read_bits_leq32(3))? as usize;

                for _ in 0..coded_components {
                    let sf_index = bits(br.read_bits_leq32(6))? as usize;
                    if component_count >= MAX_COMPONENTS {
                        return decode_error("atrac3: too many tonal components");
                    }

                    let cmp = &mut components[component_count];

                    cmp.pos = b * 64 + bits(br.read_bits_leq32(6))? as usize;

                    let max_coded_values = SAMPLES_PER_FRAME - cmp.pos;
                    let coded_values = (coded_values_per_component + 1).min(max_coded_values);

                    let scale_factor = self.sf_table[sf_index] * INV_MAX_QUANT[quant_step_index];

                    read_quant_spectral_coeffs(
                        br,
                        &self.codebooks,
                        quant_step_index,
                        coding_mode,
                        &mut mantissa,
                        coded_values,
                    )?;

                    cmp.num_coefs = coded_values;

                    // Inverse quantization.
                    for (c, &m) in cmp.coef.iter_mut().zip(&mantissa).take(coded_values) {
                        *c = m as f32 * scale_factor;
                    }

                    component_count += 1;
                }
            }
        }

        Ok(component_count)
    }

    /// Decodes a sound unit into `output`, the decoded samples before the IQMF
    /// (`decode_channel_sound_unit`).
    fn decode_channel_sound_unit(
        &mut self,
        br: &mut BitReaderLtr<'_>,
        snd: &mut ChannelUnit,
        output: &mut [f32],
        channel_num: usize,
        coding_mode: u16,
    ) -> Result<()> {
        if coding_mode == JOINT_STEREO && channel_num % 2 == 1 {
            if bits(br.read_bits_leq32(2))? != 3 {
                return decode_error("atrac3: joint stereo mono sound unit id != 3");
            }
        }
        else if bits(br.read_bits_leq32(6))? != 0x28 {
            return decode_error("atrac3: sound unit id != 0x28");
        }

        // Number of coded QMF bands.
        snd.bands_coded = bits(br.read_bits_leq32(2))? as usize;

        // The gain block of the frame that is decoded now is the next one for the compensation.
        let sw = snd.gc_blk_switch;
        decode_gain_control(br, &mut snd.gain_block[1 - sw], snd.bands_coded)?;

        snd.num_components =
            self.decode_tonal_components(br, &mut snd.components, snd.bands_coded)?;

        let num_subbands = self.decode_spectrum(br, &mut snd.spectrum)?;

        // Merge the decoded spectrum and tonal components.
        let last_tonal =
            add_tonal_components(&mut snd.spectrum, &snd.components[..snd.num_components]);

        // Calculate the number of used MLT/QMF bands according to the amount of coded spectral
        // lines.
        let mut num_bands = (usize::from(SUBBAND_TAB[num_subbands + 1]) - 1) >> 8;
        if let Some(last_tonal) = last_tonal {
            num_bands = num_bands.max((last_tonal + 256) >> 8);
        }

        // Reconstruct the time domain samples.
        for band in 0..4 {
            // Perform the IMDCT step without overlapping.
            if band <= num_bands {
                self.imlt(&mut snd.spectrum[band * 256..(band + 1) * 256], band & 1 != 0);
            }
            else {
                self.imdct_buf.fill(0.0);
            }

            // Gain compensation and overlapping.
            self.gainc.apply(
                &self.imdct_buf,
                &mut snd.prev_frame[band * 256..(band + 1) * 256],
                &snd.gain_block[sw][band],
                &snd.gain_block[1 - sw][band],
                &mut output[band * 256..(band + 1) * 256],
            );
        }

        // Swap the gain control buffers for the next frame.
        snd.gc_blk_switch ^= 1;

        Ok(())
    }

    /// Applies the inverse QMF synthesis filter bank to the four bands of a channel.
    fn synthesis(&mut self, unit: &mut ChannelUnit, out: &mut [f32]) {
        let w = &self.qmf_window;
        let temp = &mut self.temp_buf;

        // Bands 0 and 1 into 0..512.
        iqmf_prepare(&out[0..256], &out[256..512], &unit.delay_buf1, temp);
        iqmf_finish(w, 256, temp, &mut out[0..512], &mut unit.delay_buf1);

        // Bands 3 (low) and 2 (high) into 512..1024.
        iqmf_prepare(&out[768..1024], &out[512..768], &unit.delay_buf2, temp);
        iqmf_finish(w, 256, temp, &mut out[512..1024], &mut unit.delay_buf2);

        // The two halves into 0..1024.
        iqmf_prepare(&out[0..512], &out[512..1024], &unit.delay_buf3, temp);
        iqmf_finish(w, 512, temp, &mut out[0..1024], &mut unit.delay_buf3);
    }
}
