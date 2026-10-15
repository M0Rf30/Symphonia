// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VQF TwinVQ decoder.
//!
//! TwinVQ (Transform-domain Weighted Interleave Vector Quantization) is NTT's transform codec,
//! stored in `.vqf` files. There is no public specification; this decoder is a port of the
//! FFmpeg `twinvq` decoder (see `NOTICE`).
//!
//! The codec parameters must carry 12 bytes of extra data: three big-endian 32-bit integers
//! holding the number of channels minus one, the total bit rate in kbit/s and the sample rate
//! flag (the rate in kHz, `44` meaning 44.1 kHz, `22` meaning 22.05 kHz and `11` meaning
//! 11.025 kHz). These are the first 12 bytes of the `COMM` chunk of a `.vqf` file.
//!
//! Each packet holds one frame of `size` samples per channel and starts with two bytes added by
//! the demuxer: the number of bits to skip in the second byte, followed by the last byte of the
//! previous packet (frames are not byte aligned). Like the reference decoder, the output of the
//! first two packets is discarded.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]

mod modes;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TWINVQ;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use modes::{FT_LONG, FT_MEDIUM, FT_PPC, FT_SHORT, ModeTab, select_mode};

const PPC_SHAPE_CB_SIZE: usize = 64;
const PPC_SHAPE_LEN_MAX: usize = 60;
const SUB_AMP_MAX: f32 = 4500.0;
const MULAW_MU: f32 = 100.0;
const GAIN_BITS: u32 = 8;
const AMP_MAX: f32 = 13000.0;
const SUB_GAIN_BITS: u32 = 5;
const WINDOW_TYPE_BITS: u32 = 4;
const PGAIN_MU: f32 = 200.0;
const LSP_COEFS_MAX: usize = 20;
const LSP_SPLIT_MAX: usize = 4;
const CHANNELS_MAX: usize = 2;
const SUBBLOCKS_MAX: usize = 16;
const BARK_N_COEF_MAX: usize = 4;
/// The number of packets whose output is dropped, as in the reference decoder.
const DISCARDED_PACKETS: u32 = 2;

/// Window type to frame type.
const WTYPE_TO_FTYPE: [usize; 9] =
    [FT_LONG, FT_LONG, FT_SHORT, FT_LONG, FT_MEDIUM, FT_LONG, FT_LONG, FT_MEDIUM, FT_MEDIUM];

/// Window type to window size index (0: long, 1: medium, 2: short, half size).
const WTYPE_TO_WSIZE: [usize; 9] = [0, 0, 2, 2, 2, 1, 0, 1, 1];

fn truncated(_: std::io::Error) -> Error {
    Error::DecodeError("twinvq: truncated packet")
}

/// Rounded integer division as the `ROUNDED_DIV` macro of FFmpeg's `libavutil/common.h`.
fn rounded_div(a: i32, b: i32) -> i32 {
    (if a >= 0 { a + (b >> 1) } else { a - (b >> 1) }) / b
}

/// Inverse mu-law expansion; computed with the same mixed precision as the reference.
fn mulawinv(y: f32, clip: f32, mu: f32) -> f32 {
    let y = (y / clip).clamp(-1.0, 1.0);
    let sign: f32 = if y > 0.0 { 1.0 } else { -1.0 };
    let e = ((1.0 + mu) as f64).ln() * f64::from(y.abs());
    ((f64::from(clip * sign) * (e.exp() - 1.0)) / f64::from(mu)) as f32
}

/// Evaluates a single LPC amplitude spectrum envelope coefficient from the cosines of the line
/// spectrum pairs. `order` must be a multiple of four.
fn eval_lpc_spectrum(lsp: &[f32], cos_val: f32, order: usize) -> f32 {
    let mut p = 0.5f32;
    let mut q = 0.5f32;
    let two_cos_w = 2.0 * cos_val;

    for c in lsp[..order].chunks_exact(4) {
        q *= c[0] - two_cos_w;
        p *= c[1] - two_cos_w;

        q *= c[2] - two_cos_w;
        p *= c[3] - two_cos_w;
    }

    // Squares of p and q scaled by the real-root factors, as in the reference (`p *= p * ...`).
    #[allow(clippy::misrefactored_assign_op)]
    {
        p *= p * (2.0 - two_cos_w);
        q *= q * (2.0 + two_cos_w);
    }

    0.5 / (p + q)
}

/// Linearly interpolates `size` values strictly between `v2` and `v1`.
fn interpolate(out: &mut [f32], v1: f32, mut v2: f32, size: usize) {
    let step = (v1 - v2) / (size + 1) as f32;

    for o in &mut out[..size] {
        v2 += step;
        *o = v2;
    }
}

fn get_cos(idx: usize, part: bool, cos_tab: &[f32], size: usize) -> f32 {
    if part { -cos_tab[size - idx - 1] } else { cos_tab[idx] }
}

/// Evaluates the LPC amplitude spectrum envelope of one half of a long/medium frame. Every
/// `step`th value is evaluated, the others are interpolated (or evaluated, depending on the
/// local curvature).
fn eval_lpcenv_or_interp(
    mtab: &ModeTab,
    cos_tab: &[f32],
    out: &mut [f32],
    input: &[f32],
    size: usize,
    step: usize,
    part: bool,
) {
    let n_lsp = mtab.n_lsp;

    // Fill the 's'.
    for i in (0..size).step_by(step) {
        out[i] = eval_lpc_spectrum(input, get_cos(i, part, cos_tab, size), n_lsp);
    }

    // Fill the 'iiiibiiii'.
    let mut i = step;
    while i + 2 * step <= size {
        if f64::from(out[i + step] + out[i - step]) > 1.95 * f64::from(out[i])
            || out[i + step] >= out[i - step]
        {
            let (v1, v2) = (out[i], out[i - step]);
            interpolate(&mut out[i - step + 1..], v1, v2, step - 1);
        }
        else {
            out[i - step / 2] =
                eval_lpc_spectrum(input, get_cos(i - step / 2, part, cos_tab, size), n_lsp);
            let (v1, v2) = (out[i - step / 2], out[i - step]);
            interpolate(&mut out[i - step + 1..], v1, v2, step / 2 - 1);
            let (v1, v2) = (out[i], out[i - step / 2]);
            interpolate(&mut out[i - step / 2 + 1..], v1, v2, step / 2 - 1);
        }
        i += step;
    }

    let (v1, v2) = (out[size - step], out[size - 2 * step]);
    interpolate(&mut out[size - 2 * step + 1..], v1, v2, step - 1);
}

fn eval_lpcenv_2parts(
    mtab: &ModeTab,
    cos_tab: &[f32],
    buf: &[f32],
    lpc: &mut [f32],
    size: usize,
    step: usize,
) {
    eval_lpcenv_or_interp(mtab, cos_tab, lpc, buf, size / 2, step, false);
    eval_lpcenv_or_interp(mtab, cos_tab, &mut lpc[size / 2..], buf, size / 2, 2 * step, true);

    let (v1, v2) = (lpc[size / 2], lpc[size / 2 - step]);
    interpolate(&mut lpc[size / 2 - step + 1..], v1, v2, step);

    let v = lpc[size - 2 * step];
    lpc[size - 2 * step + 1..size].fill(v);
}

/// Rearranges the LSP coefficients so that they have a minimum distance of `min_dist`, as
/// described in section 3.2.4 of the G.729 specification.
fn rearrange_lsp(order: usize, lsp: &mut [f32], min_dist: f32) {
    let min_dist2 = min_dist * 0.5;

    for i in 1..order {
        if lsp[i] - lsp[i - 1] < min_dist {
            let avg = (lsp[i] + lsp[i - 1]) * 0.5;

            lsp[i - 1] = avg - min_dist2;
            lsp[i] = avg + min_dist2;
        }
    }
}

/// Decodes the LSP coefficients of one channel and updates the LSP history.
fn decode_lsp(
    mtab: &ModeTab,
    lpc_idx1: usize,
    lpc_idx2: &[u8],
    lpc_hist_idx: usize,
    lsp: &mut [f32],
    hist: &mut [f32],
) {
    let n = mtab.n_lsp;

    let cb = mtab.lsp_codebook;
    let cb2 = &cb[(1 << mtab.lsp_bit1) * n..];
    let cb3 = &cb2[(1 << mtab.lsp_bit2) * n..];

    let funny_rounding: [i32; 4] =
        [-2, if mtab.lsp_split == 4 { -2 } else { 1 }, if mtab.lsp_split == 4 { -2 } else { 1 }, 0];

    let mut j = 0;
    for i in 0..mtab.lsp_split {
        let chunk_end = (((i + 1) * n) as i32 + funny_rounding[i]) as usize / mtab.lsp_split;
        while j < chunk_end {
            lsp[j] = cb[lpc_idx1 * n + j] + cb2[usize::from(lpc_idx2[i]) * n + j];
            j += 1;
        }
    }

    rearrange_lsp(n, lsp, 0.0001);

    for i in 0..n {
        let c = cb3[lpc_hist_idx * n + i];
        let tmp1 = 1.0 - c;
        let tmp2 = hist[i] * c;
        hist[i] = lsp[i];
        lsp[i] = lsp[i] * tmp1 + tmp2;
    }

    rearrange_lsp(n, lsp, 0.0001);
    rearrange_lsp(n, lsp, 0.000095);

    // Sort the nearly sorted values (insertion sort).
    for i in 0..n - 1 {
        let mut j = i as isize;
        while j >= 0 && lsp[j as usize] > lsp[j as usize + 1] {
            lsp.swap(j as usize, j as usize + 1);
            j -= 1;
        }
    }
}

/// Parameters of the bitstream layout derived from the mode, the bit rate and the channel count.
struct BsParams {
    /// Number of vector pairs per frame type.
    n_div: [usize; 4],
    /// Main codebook vector lengths, before and after `length_change`.
    length: [[usize; 2]; 4],
    length_change: [usize; 4],
    /// Bits of the two main codebook indices, before and after `bits_main_spec_change`.
    bits_main_spec: [[[u32; 2]; 4]; 2],
    bits_main_spec_change: [usize; 4],
    /// Spectrum coefficient permutation per frame type.
    permut: Vec<[i16; 4096]>,
}

impl BsParams {
    fn new(mtab: &ModeTab, channels: usize, total_fr_bits: i64) -> Result<Self> {
        let n_ch = channels as i64;

        let lsp_bits_per_block =
            n_ch * i64::from(mtab.lsp_bit0 + mtab.lsp_bit1 + mtab.lsp_split as u32 * mtab.lsp_bit2);

        let ppc_bits = n_ch * i64::from(mtab.pgain_bit + mtab.ppc_shape_bit + mtab.ppc_period_bit);

        let mut bse_bits = [0i64; 3];
        for (i, b) in bse_bits.iter_mut().enumerate() {
            // +1 for the history usage switch.
            *b = n_ch * ((mtab.fmode[i].bark_n_coef as u32 * mtab.fmode[i].bark_n_bit + 1) as i64);
        }

        let mut bsize_no_main_cb = [0i64; 3];
        bsize_no_main_cb[2] = bse_bits[2]
            + lsp_bits_per_block
            + ppc_bits
            + i64::from(WINDOW_TYPE_BITS)
            + n_ch * i64::from(GAIN_BITS);

        for i in 0..2 {
            bsize_no_main_cb[i] = lsp_bits_per_block
                + n_ch * i64::from(GAIN_BITS)
                + i64::from(WINDOW_TYPE_BITS)
                + mtab.fmode[i].sub as i64 * (bse_bits[i] + n_ch * i64::from(SUB_GAIN_BITS));
        }

        let mut p = BsParams {
            n_div: [0; 4],
            length: [[0; 2]; 4],
            length_change: [0; 4],
            bits_main_spec: [[[0; 2]; 4]; 2],
            bits_main_spec_change: [0; 4],
            permut: vec![[0i16; 4096]; 4],
        };

        // The remaining bits are all used for the main spectrum coefficients.
        #[allow(clippy::needless_range_loop)]
        for i in 0..4 {
            let (bit_size, vect_size) = if i == 3 {
                (n_ch * i64::from(mtab.ppc_shape_bit), n_ch * mtab.ppc_shape_len as i64)
            }
            else {
                (total_fr_bits - bsize_no_main_cb[i], n_ch * mtab.size as i64)
            };

            if bit_size <= 0 {
                return unsupported_error("twinvq: invalid bit rate for the frame size");
            }

            let n_div = (bit_size + 13) / 14;
            p.n_div[i] = n_div as usize;

            let rounded_up = (bit_size + n_div - 1) / n_div;
            let rounded_down = bit_size / n_div;
            let num_rounded_down = rounded_up * n_div - bit_size;
            let num_rounded_up = n_div - num_rounded_down;
            p.bits_main_spec[0][i][0] = ((rounded_up + 1) / 2) as u32;
            p.bits_main_spec[1][i][0] = (rounded_up / 2) as u32;
            p.bits_main_spec[0][i][1] = ((rounded_down + 1) / 2) as u32;
            p.bits_main_spec[1][i][1] = (rounded_down / 2) as u32;
            p.bits_main_spec_change[i] = num_rounded_up as usize;

            let rounded_up = (vect_size + n_div - 1) / n_div;
            let rounded_down = vect_size / n_div;
            let num_rounded_down = rounded_up * n_div - vect_size;
            let num_rounded_up = n_div - num_rounded_down;
            p.length[i][0] = rounded_up as usize;
            p.length[i][1] = rounded_down as usize;
            p.length_change[i] = num_rounded_up as usize;
        }

        // The temporary permutation table is shared between the frame types and is not cleared
        // in between, like the (zero initialized) scratch buffer of the reference decoder.
        let mut tmp_perm = [0i16; 4096];
        for ftype in FT_SHORT..=FT_PPC {
            p.construct_perm_table(mtab, channels, ftype, &mut tmp_perm);
        }

        Ok(p)
    }

    fn construct_perm_table(
        &mut self,
        mtab: &ModeTab,
        channels: usize,
        ftype: usize,
        tmp_perm: &mut [i16; 4096],
    ) {
        let (size, block_size) = if ftype == FT_PPC {
            (channels, mtab.ppc_shape_len)
        }
        else {
            (channels * mtab.fmode[ftype].sub, mtab.size / mtab.fmode[ftype].sub)
        };

        let num_vect = self.n_div[ftype];
        let line_len = self.length[ftype];
        let length_div = self.length_change[ftype];

        // permutate_in_line(): interpret the data as a num_blocks x line_len[0] matrix and
        // cyclically permute each line.
        for i in 0..line_len[0] {
            let shift = if size == 1
                || (ftype == FT_LONG && num_vect % size != 0)
                || (ftype != FT_LONG && num_vect & 1 != 0)
                || i == line_len[1]
            {
                0
            }
            else if ftype == FT_LONG {
                i
            }
            else {
                i * i
            };

            let mut j = 0;
            while j < num_vect && j + num_vect * i < block_size * size {
                tmp_perm[i * num_vect + j] = (i * num_vect + (j + shift) % num_vect) as i16;
                j += 1;
            }
        }

        // transpose_perm()
        let out = &mut self.permut[ftype];
        let mut cont = 0;
        for i in 0..num_vect {
            for j in 0..line_len[usize::from(i >= length_div)] {
                out[cont] = tmp_perm[j * num_vect + i];
                cont += 1;
            }
        }

        // linear_perm()
        let n_blocks = size as i16;
        let block = block_size as i16;
        for v in &mut out[..size * block_size] {
            *v = block * (*v % n_blocks) + *v / n_blocks;
        }
    }
}

/// The parsed fields of one frame.
struct FrameData {
    window_type: usize,
    ftype: usize,
    main_coeffs: [u8; 1024],
    ppc_coeffs: [u8; PPC_SHAPE_LEN_MAX],
    gain_bits: [u8; CHANNELS_MAX],
    sub_gain_bits: [u8; CHANNELS_MAX * SUBBLOCKS_MAX],
    bark1: [[[u8; BARK_N_COEF_MAX]; SUBBLOCKS_MAX]; CHANNELS_MAX],
    bark_use_hist: [[bool; SUBBLOCKS_MAX]; CHANNELS_MAX],
    lpc_idx1: [u8; CHANNELS_MAX],
    lpc_idx2: [[u8; LSP_SPLIT_MAX]; CHANNELS_MAX],
    lpc_hist_idx: [u8; CHANNELS_MAX],
    p_coef: [u32; CHANNELS_MAX],
    g_coef: [u32; CHANNELS_MAX],
}

impl FrameData {
    fn new() -> Self {
        FrameData {
            window_type: 0,
            ftype: FT_LONG,
            main_coeffs: [0; 1024],
            ppc_coeffs: [0; PPC_SHAPE_LEN_MAX],
            gain_bits: [0; CHANNELS_MAX],
            sub_gain_bits: [0; CHANNELS_MAX * SUBBLOCKS_MAX],
            bark1: [[[0; BARK_N_COEF_MAX]; SUBBLOCKS_MAX]; CHANNELS_MAX],
            bark_use_hist: [[false; SUBBLOCKS_MAX]; CHANNELS_MAX],
            lpc_idx1: [0; CHANNELS_MAX],
            lpc_idx2: [[0; LSP_SPLIT_MAX]; CHANNELS_MAX],
            lpc_hist_idx: [0; CHANNELS_MAX],
            p_coef: [0; CHANNELS_MAX],
            g_coef: [0; CHANNELS_MAX],
        }
    }
}

fn read_bits(br: &mut BitReaderLtr<'_>, n: u32) -> Result<u32> {
    if n == 0 { Ok(0) } else { br.read_bits_leq32(n).map_err(truncated) }
}

/// Reads the main codebook indices of `n_div` vector pairs.
fn read_cb_data(
    br: &mut BitReaderLtr<'_>,
    bs: &BsParams,
    dst: &mut [u8],
    ftype: usize,
) -> Result<()> {
    for i in 0..bs.n_div[ftype] {
        let second = usize::from(i >= bs.bits_main_spec_change[ftype]);

        dst[2 * i] = read_bits(br, bs.bits_main_spec[0][ftype][second])? as u8;
        dst[2 * i + 1] = read_bits(br, bs.bits_main_spec[1][ftype][second])? as u8;
    }
    Ok(())
}

/// Inverse quantization. Reads the codebook entries for both codebooks, sums the corresponding
/// vectors and writes the result to `out` after permutation.
fn dequant(
    bs: &BsParams,
    cb_bits: &[u8],
    out: &mut [f32],
    ftype: usize,
    cb0: &[i16],
    cb1: &[i16],
    cb_len: usize,
) -> Result<()> {
    let mut pos = 0;

    for i in 0..bs.n_div[ftype] {
        let length = bs.length[ftype][usize::from(i >= bs.length_change[ftype])];
        let second = usize::from(i >= bs.bits_main_spec_change[ftype]);

        let mut vals = [0usize; 2];
        let mut signs = [1i32; 2];
        for k in 0..2 {
            let mut v = usize::from(cb_bits[2 * i + k]);
            if bs.bits_main_spec[k][ftype][second] == 7 {
                if v & 0x40 != 0 {
                    signs[k] = -1;
                }
                v &= 0x3f;
            }
            vals[k] = v;
        }

        let range = |v: usize| v * cb_len..v * cb_len + length;
        let (Some(tab0), Some(tab1)) = (cb0.get(range(vals[0])), cb1.get(range(vals[1])))
        else {
            return decode_error("twinvq: codebook index out of range");
        };
        let Some(perm) = bs.permut[ftype].get(pos..pos + length)
        else {
            return decode_error("twinvq: permutation out of range");
        };

        for j in 0..length {
            let Some(o) = out.get_mut(perm[j] as usize)
            else {
                return decode_error("twinvq: permutation out of range");
            };
            *o = (signs[0] * i32::from(tab0[j]) + signs[1] * i32::from(tab1[j])) as f32;
        }

        pos += length;
    }
    Ok(())
}

/// Port of the `very_broken_op` function of FFmpeg's `twinvqdec.c`: evaluates `a * b / 400`
/// rounded to the nearest integer, emulating the broken float-based calculation of the binary
/// decoder in the ambiguous cases.
fn very_broken_op(a: i32, b: i32) -> Result<i32> {
    let x = a * b + 200;

    if x % 400 != 0 || b % 5 != 0 {
        return Ok(x / 400);
    }

    let x = x / 400;

    let Some((size, rtab)) = modes::rounding_table((b / 5) as usize)
    else {
        return decode_error("twinvq: invalid PPC peak position");
    };
    let size = size as i32;

    let log2 = if (2 * (x - 1) / size) > 0 { (2 * (x - 1) / size).ilog2() as i32 } else { 0 };
    match rtab.get((size * log2 + (x - 1) % size) as usize) {
        Some(&r) => Ok(x - i32::from(r)),
        None => decode_error("twinvq: invalid PPC peak position"),
    }
}

/// Sums to `speech` a periodic peak of a given period, width and shape.
fn add_peak(
    period: i32,
    width: i32,
    shape: &[f32],
    ppc_gain: f32,
    speech: &mut [f32],
    len: usize,
) -> Result<()> {
    let bad = || decode_error("twinvq: PPC peak out of range");

    let mut shape = shape[..len].iter();
    let add = |speech: &mut [f32], idx: i32, shape_val: f32| -> Result<()> {
        match usize::try_from(idx).ok().and_then(|i| speech.get_mut(i)) {
            Some(s) => {
                *s += ppc_gain * shape_val;
                Ok(())
            }
            None => bad(),
        }
    };

    // The first peak is centered around zero.
    for i in 0..width / 2 {
        if let Some(&s) = shape.next() {
            add(speech, i, s)?;
        }
    }

    let n_peaks = rounded_div(len as i32, width);
    let mut i = 1;
    while i < n_peaks {
        let center = very_broken_op(period, i)?;
        for j in -(width / 2)..(width + 1) / 2 {
            if let Some(&s) = shape.next() {
                add(speech, j + center, s)?;
            }
        }
        i += 1;
    }

    // For the last block, be careful not to go beyond the end of the shape.
    let center = very_broken_op(period, i)?;
    for j in -(width / 2)..(width + 1) / 2 {
        match shape.next() {
            Some(&s) => add(speech, j + center, s)?,
            None => break,
        }
    }
    Ok(())
}

/// The sine window of length `n` (FFmpeg `ff_sine_window_init`).
fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i as f64 + 0.5) * (std::f64::consts::PI / (2.0 * n as f64))) as f32)
        .map(f32::sin)
        .collect()
}

/// VQF TwinVQ decoder.
pub struct TwinVqDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    mtab: &'static ModeTab,
    channels: usize,
    sample_rate: u32,
    /// Bit rate in bit/s.
    bit_rate: u32,
    /// Minimum packet length in bytes.
    block_align: usize,
    bs: BsParams,
    imdct: [Imdct; 3],
    /// Sine windows for long, medium and short (half size) windows.
    windows: [Vec<f32>; 3],
    cos_tabs: [Vec<f32>; 3],
    bits: FrameData,

    // History.
    lsp_hist: [[f32; LSP_COEFS_MAX]; CHANNELS_MAX],
    bark_hist: [[[f32; 40]; CHANNELS_MAX]; 3],
    prev_frame: Vec<f32>,
    curr_frame: Vec<f32>,
    last_block_pos: usize,
    discarded_packets: u32,

    // Scratch.
    spectrum: Vec<f32>,
    tmp_buf: Vec<f32>,
    imdct_buf: Vec<f32>,
    imdct_out: Vec<f32>,
    ppc_shape: [f32; PPC_SHAPE_LEN_MAX * CHANNELS_MAX * 4],
    /// De-interleaved output of one frame.
    out: Vec<f32>,
}

impl TwinVqDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_TWINVQ {
            return unsupported_error("twinvq: invalid codec");
        }

        let extra = match &params.extra_data {
            Some(extra) if extra.len() >= 12 => extra,
            _ => return unsupported_error("twinvq: missing or incomplete extra data"),
        };

        let be32 =
            |o: usize| u32::from_be_bytes([extra[o], extra[o + 1], extra[o + 2], extra[o + 3]]);

        let channels = be32(0).saturating_add(1);
        let kbps = be32(4);
        let isampf = be32(8);

        if isampf < 8 || isampf > 44 {
            return unsupported_error("twinvq: unsupported sample rate");
        }

        let sample_rate = match isampf {
            44 => 44100,
            22 => 22050,
            11 => 11025,
            _ => isampf * 1000,
        };

        if channels == 0 || channels as usize > CHANNELS_MAX {
            return unsupported_error("twinvq: unsupported number of channels");
        }
        let channels = channels as usize;

        let ibps = kbps / channels as u32;
        if ibps < 8 || ibps > 48 {
            return unsupported_error("twinvq: bad bit rate per channel");
        }

        let Some(mtab) = select_mode(isampf, ibps)
        else {
            return unsupported_error("twinvq: unsupported sample rate and bit rate combination");
        };

        let bit_rate = kbps * 1000;
        let size = mtab.size;

        let total_fr_bits = i64::from(bit_rate) * size as i64 / i64::from(sample_rate);
        let frame_size = total_fr_bits as usize + 8;
        let block_align = (frame_size + 7) >> 3;

        let bs = BsParams::new(mtab, channels, total_fr_bits)?;

        // MDCT, windows and cosine tables.
        let norm = if channels == 1 { 2.0 } else { 1.0 };
        let bsize = |i: usize| size / mtab.fmode[i].sub;
        let imdct = [
            Imdct::new_scaled(bsize(0), (norm / bsize(0) as f64).sqrt() / 32768.0),
            Imdct::new_scaled(bsize(1), (norm / bsize(1) as f64).sqrt() / 32768.0),
            Imdct::new_scaled(bsize(2), (norm / bsize(2) as f64).sqrt() / 32768.0),
        ];

        let windows = [
            sine_window(size / mtab.fmode[FT_LONG].sub),
            sine_window(size / mtab.fmode[FT_MEDIUM].sub),
            sine_window(size / (mtab.fmode[FT_SHORT].sub * 2)),
        ];

        let make_cos_tab = |i: usize| {
            let m = 4 * size / mtab.fmode[i].sub;
            let freq = 2.0 * std::f64::consts::PI / m as f64;
            let mut tab = vec![0f32; m / 4];
            #[allow(clippy::needless_range_loop)]
            for j in 0..=m / 8 {
                tab[j] = (((2 * j + 1) as f64) * freq).cos() as f32;
            }
            for j in 1..m / 8 {
                tab[m / 4 - j] = tab[j];
            }
            tab
        };
        let cos_tabs = [make_cos_tab(0), make_cos_tab(1), make_cos_tab(2)];

        let table_size = 2 * size * channels;

        // The reference initializes only the first three values of the bark history to 0.1
        // (FF_ARRAY_ELEMS of the 3-D array); the rest of the history starts at zero. Keep that
        // so the output matches.
        let mut bark_hist = [[[0f32; 40]; CHANNELS_MAX]; 3];
        bark_hist[0][0][..3].fill(0.1);

        let channel_layout = if channels == 1 {
            layouts::CHANNEL_LAYOUT_MONO
        }
        else {
            layouts::CHANNEL_LAYOUT_STEREO
        };
        let spec = AudioSpec::new(sample_rate, channel_layout.clone());
        let buf = AudioBuffer::new(spec, size);

        let mut params = params.clone();
        params.with_sample_rate(sample_rate).with_channels(channel_layout);
        params.with_max_frames_per_packet(size as u64);

        Ok(TwinVqDecoder {
            params,
            opts: *opts,
            buf,
            mtab,
            channels,
            sample_rate,
            bit_rate,
            block_align,
            bs,
            imdct,
            windows,
            cos_tabs,
            bits: FrameData::new(),
            lsp_hist: [[0.0; LSP_COEFS_MAX]; CHANNELS_MAX],
            bark_hist,
            prev_frame: vec![0.0; table_size + size],
            curr_frame: vec![0.0; table_size + size],
            last_block_pos: 0,
            discarded_packets: 0,
            spectrum: vec![0.0; table_size],
            tmp_buf: vec![0.0; size],
            imdct_buf: vec![0.0; 2 * size],
            imdct_out: vec![0.0; size],
            ppc_shape: [0.0; PPC_SHAPE_LEN_MAX * CHANNELS_MAX * 4],
            out: vec![0.0; size * channels],
        })
    }

    fn reset_state(&mut self) {
        self.lsp_hist = [[0.0; LSP_COEFS_MAX]; CHANNELS_MAX];
        self.bark_hist = [[[0.0; 40]; CHANNELS_MAX]; 3];
        self.bark_hist[0][0][..3].fill(0.1);
        self.prev_frame.fill(0.0);
        self.curr_frame.fill(0.0);
        self.last_block_pos = 0;
        self.discarded_packets = 0;
    }

    fn read_bitstream(&mut self, data: &[u8]) -> Result<()> {
        let mtab = self.mtab;
        let channels = self.channels;
        let bs = &self.bs;
        let bits = &mut self.bits;

        let mut br = BitReaderLtr::new(data);
        let skip = br.read_bits_leq32(8).map_err(truncated)?;
        br.ignore_bits(skip).map_err(truncated)?;

        let window_type = read_bits(&mut br, WINDOW_TYPE_BITS)? as usize;
        if window_type > 8 {
            return decode_error("twinvq: invalid window type");
        }
        bits.window_type = window_type;
        bits.ftype = WTYPE_TO_FTYPE[window_type];
        let ftype = bits.ftype;

        let fm = &mtab.fmode[ftype];
        let sub = fm.sub;

        read_cb_data(&mut br, bs, &mut bits.main_coeffs, ftype)?;

        for i in 0..channels {
            for j in 0..sub {
                for k in 0..fm.bark_n_coef {
                    bits.bark1[i][j][k] = read_bits(&mut br, fm.bark_n_bit)? as u8;
                }
            }
        }

        for i in 0..channels {
            for j in 0..sub {
                bits.bark_use_hist[i][j] = read_bits(&mut br, 1)? != 0;
            }
        }

        if ftype == FT_LONG {
            for i in 0..channels {
                bits.gain_bits[i] = read_bits(&mut br, GAIN_BITS)? as u8;
            }
        }
        else {
            for i in 0..channels {
                bits.gain_bits[i] = read_bits(&mut br, GAIN_BITS)? as u8;
                for j in 0..sub {
                    bits.sub_gain_bits[i * sub + j] = read_bits(&mut br, SUB_GAIN_BITS)? as u8;
                }
            }
        }

        for i in 0..channels {
            bits.lpc_hist_idx[i] = read_bits(&mut br, mtab.lsp_bit0)? as u8;
            bits.lpc_idx1[i] = read_bits(&mut br, mtab.lsp_bit1)? as u8;

            for j in 0..mtab.lsp_split {
                bits.lpc_idx2[i][j] = read_bits(&mut br, mtab.lsp_bit2)? as u8;
            }
        }

        if ftype == FT_LONG {
            read_cb_data(&mut br, bs, &mut bits.ppc_coeffs, FT_PPC)?;
            for i in 0..channels {
                bits.p_coef[i] = read_bits(&mut br, mtab.ppc_period_bit)?;
                bits.g_coef[i] = read_bits(&mut br, mtab.pgain_bit)?;
            }
        }

        Ok(())
    }

    /// Computes the gains of the frame, `[channel * sub + sub-block]`.
    fn dec_gain(&self, ftype: usize, out: &mut [f32]) {
        let sub = self.mtab.fmode[ftype].sub;
        let step = (f64::from(AMP_MAX) / f64::from((1 << GAIN_BITS) - 1)) as f32;
        let sub_step = (f64::from(SUB_AMP_MAX) / f64::from((1 << SUB_GAIN_BITS) - 1)) as f32;

        let gain_arg = |gb: u8| (f64::from(step) * 0.5 + f64::from(step * f32::from(gb))) as f32;

        if ftype == FT_LONG {
            #[allow(clippy::needless_range_loop)]
            for i in 0..self.channels {
                out[i] = ((1.0 / f64::from(1 << 13))
                    * f64::from(mulawinv(gain_arg(self.bits.gain_bits[i]), AMP_MAX, MULAW_MU)))
                    as f32;
            }
        }
        else {
            for i in 0..self.channels {
                let val = (1.0 / f64::from(1 << 23))
                    * f64::from(mulawinv(gain_arg(self.bits.gain_bits[i]), AMP_MAX, MULAW_MU));

                for j in 0..sub {
                    let sg = f64::from(sub_step) * 0.5
                        + f64::from(sub_step * f32::from(self.bits.sub_gain_bits[i * sub + j]));
                    out[i * sub + j] =
                        (val * f64::from(mulawinv(sg as f32, SUB_AMP_MAX, MULAW_MU))) as f32;
                }
            }
        }
    }

    /// Decodes the bark scale envelope into `tmp_buf`.
    fn dec_bark_env(&mut self, ch: usize, j: usize, ftype: usize, gain: f32) {
        let fm = &self.mtab.fmode[ftype];
        let hist = &mut self.bark_hist[ftype][ch];
        let val: f32 = [0.4, 0.35, 0.28][ftype];
        let use_hist = self.bits.bark_use_hist[ch][j];
        let input = &self.bits.bark1[ch][j];
        let fw_cb_len = fm.bark_env_size / fm.bark_n_coef;

        let mut idx = 0;
        let mut pos = 0;
        for i in 0..fw_cb_len {
            for &inp in &input[..fm.bark_n_coef] {
                let tmp2 = (f64::from(fm.bark_cb[fw_cb_len * usize::from(inp) + i])
                    * (1.0 / 4096.0)) as f32;
                let mut st = if use_hist {
                    ((1.0 - f64::from(val)) * f64::from(tmp2)
                        + f64::from(val) * f64::from(hist[idx])
                        + 1.0) as f32
                }
                else {
                    tmp2 + 1.0
                };

                hist[idx] = tmp2;
                if st < -1.0 {
                    st = 1.0;
                }

                let n = usize::from(fm.bark_tab[idx]);
                self.tmp_buf[pos..pos + n].fill(st * gain);
                pos += n;
                idx += 1;
            }
        }
    }

    /// Adds the periodic peak component to the spectrum of the channel.
    fn decode_ppc(&mut self, ch: usize, shape_off: usize) -> Result<()> {
        let mtab = self.mtab;
        let isampf = (self.sample_rate / 1000) as i32;
        let ibps = (self.bit_rate / (1000 * self.channels as u32)) as i32;
        let size = mtab.size as i32;
        let min_period = rounded_div(40 * 2 * size, isampf);
        let max_period = rounded_div(40 * 2 * size * 6, isampf);
        let period_range = max_period - min_period;
        let pgain_step = (25000.0f64 / f64::from((1u32 << mtab.pgain_bit) - 1)) as f32;
        let g_coef = self.bits.g_coef[ch];
        let ppc_gain = ((1.0 / 8192.0)
            * f64::from(mulawinv(pgain_step * g_coef as f32 + pgain_step / 2.0, 25000.0, PGAIN_MU)))
            as f32;

        // This is actually the period multiplied by 400. It is just linearly coded between its
        // maximum and minimum value.
        let period = min_period
            + rounded_div(
                self.bits.p_coef[ch] as i32 * period_range,
                (1 << mtab.ppc_period_bit) - 1,
            );

        let width = if isampf == 22 && ibps == 32 {
            // For some unknown reason, NTT decided to code this case differently...
            rounded_div((period + 800) * mtab.peak_per2wid, 400 * size)
        }
        else {
            period * mtab.peak_per2wid / (400 * size)
        };

        if width <= 0 {
            return decode_error("twinvq: invalid PPC peak width");
        }

        let shape = &self.ppc_shape[shape_off..];
        let speech = &mut self.spectrum[mtab.size * ch..mtab.size * (ch + 1)];
        add_peak(period, width, shape, ppc_gain, speech, mtab.ppc_shape_len)
    }

    fn dec_lpc_spectrum_inv(&mut self, lsp: &mut [f32], ftype: usize) {
        let mtab = self.mtab;
        let size = mtab.size / mtab.fmode[ftype].sub;

        for l in &mut lsp[..mtab.n_lsp] {
            *l = (2.0 * f64::from(*l).cos()) as f32;
        }

        let cos_tab = &self.cos_tabs[ftype];
        let lpc = &mut self.tmp_buf;
        match ftype {
            FT_LONG => eval_lpcenv_2parts(mtab, cos_tab, lsp, lpc, size, 8),
            FT_MEDIUM => eval_lpcenv_2parts(mtab, cos_tab, lsp, lpc, size, 2),
            _ => {
                let size_s = size;
                for i in 0..size_s / 2 {
                    let cos_i = self.cos_tabs[0][i];
                    lpc[i] = eval_lpc_spectrum(lsp, cos_i, mtab.n_lsp);
                    lpc[size_s - i - 1] = eval_lpc_spectrum(lsp, -cos_i, mtab.n_lsp);
                }
            }
        }
    }

    fn read_and_decode_spectrum(&mut self, ftype: usize) -> Result<()> {
        let mtab = self.mtab;
        let channels = self.channels;
        let fm = &mtab.fmode[ftype];
        let sub = fm.sub;
        let block_size = mtab.size / sub;
        let mut gain = [0f32; CHANNELS_MAX * SUBBLOCKS_MAX];

        dequant(
            &self.bs,
            &self.bits.main_coeffs,
            &mut self.spectrum,
            ftype,
            fm.cb0,
            fm.cb1,
            fm.cb_len_read,
        )?;

        self.dec_gain(ftype, &mut gain);

        if ftype == FT_LONG {
            let n_div = self.bs.n_div[3];
            let cb_len_p = (n_div + mtab.ppc_shape_len * channels - 1) / n_div;
            let cb = mtab.ppc_shape_cb;
            let Some(cb1) = cb.get(cb_len_p * PPC_SHAPE_CB_SIZE..)
            else {
                return decode_error("twinvq: invalid PPC codebook");
            };
            dequant(
                &self.bs,
                &self.bits.ppc_coeffs,
                &mut self.ppc_shape,
                FT_PPC,
                cb,
                cb1,
                cb_len_p,
            )?;
        }

        for i in 0..channels {
            for j in 0..sub {
                self.dec_bark_env(i, j, ftype, gain[sub * i + j]);

                let chunk = &mut self.spectrum[mtab.size * i + block_size * j..][..block_size];
                for (c, t) in chunk.iter_mut().zip(&self.tmp_buf[..block_size]) {
                    *c *= *t;
                }
            }

            if ftype == FT_LONG {
                self.decode_ppc(i, i * mtab.ppc_shape_len)?;
            }

            let mut lsp = [0f32; LSP_COEFS_MAX];
            decode_lsp(
                mtab,
                usize::from(self.bits.lpc_idx1[i]),
                &self.bits.lpc_idx2[i],
                usize::from(self.bits.lpc_hist_idx[i]),
                &mut lsp,
                &mut self.lsp_hist[i],
            );

            self.dec_lpc_spectrum_inv(&mut lsp, ftype);

            for j in 0..sub {
                let chunk = &mut self.spectrum[mtab.size * i + block_size * j..][..block_size];
                for (c, t) in chunk.iter_mut().zip(&self.tmp_buf[..block_size]) {
                    *c *= *t;
                }
            }
        }

        Ok(())
    }

    /// Inverse MDCT and windowed overlap-add of one channel into the current frame buffer.
    /// `prev_off` is the offset of the channel in the previous frame buffer.
    fn imdct_and_window(&mut self, ftype: usize, wtype: usize, ch: usize, prev_off: usize) {
        let mtab = self.mtab;
        let sub = mtab.fmode[ftype].sub;
        let bsize = mtab.size / sub;
        let size = mtab.size;
        let types_sizes = [
            size / mtab.fmode[FT_LONG].sub,
            size / mtab.fmode[FT_MEDIUM].sub,
            size / (mtab.fmode[FT_SHORT].sub * 2),
        ];

        let first_wsize = types_sizes[WTYPE_TO_WSIZE[wtype]];

        // The overlapped part comes from the previous frame for the first sub-block and from
        // the previous sub-block (in `imdct_out`) for the others.
        let mut prev_is_frame = true;
        let mut prev_pos = prev_off + (size - bsize) / 2;
        let mut out_pos = 2 * ch * size;

        for j in 0..sub {
            let mut sub_wtype = if ftype == FT_MEDIUM { 8 } else { wtype };

            if j == 0 && wtype == 4 {
                sub_wtype = 4;
            }
            else if j == sub - 1 && wtype == 7 {
                sub_wtype = 7;
            }

            let widx = WTYPE_TO_WSIZE[sub_wtype];
            let wsize = types_sizes[widx];
            let window = &self.windows[widx];

            let spec = &self.spectrum[size * ch + bsize * j..][..bsize];
            let imdct_buf = &mut self.imdct_buf[..2 * bsize];
            self.imdct[ftype].imdct(spec, imdct_buf);
            // The IMDCT of this crate yields 2N samples; the middle N are the unique ones.
            let buf1_off = bsize * j;
            self.imdct_out[buf1_off..buf1_off + bsize]
                .copy_from_slice(&imdct_buf[bsize / 2..bsize / 2 + bsize]);

            // vector_fmul_window(out2, prev_buf + (bsize - wsize) / 2, buf1, window, wsize / 2)
            let len = wsize / 2;
            let src0 = prev_pos + (bsize - wsize) / 2;
            for k in 0..len {
                let s0 = if prev_is_frame {
                    self.prev_frame[src0 + k]
                }
                else {
                    self.imdct_out[src0 + k]
                };
                let s1 = self.imdct_out[buf1_off + len - 1 - k];
                let wi = window[k];
                let wj = window[2 * len - 1 - k];
                self.curr_frame[out_pos + k] = s0 * wj - s1 * wi;
                self.curr_frame[out_pos + 2 * len - 1 - k] = s0 * wi + s1 * wj;
            }
            out_pos += wsize;

            let n = bsize - wsize / 2;
            self.curr_frame[out_pos..out_pos + n]
                .copy_from_slice(&self.imdct_out[buf1_off + wsize / 2..buf1_off + wsize / 2 + n]);

            out_pos += if ftype == FT_MEDIUM { (bsize - wsize) / 2 } else { bsize - wsize };

            prev_is_frame = false;
            prev_pos = buf1_off + bsize / 2;
        }

        self.last_block_pos = (size + first_wsize) / 2;
    }

    /// Inverse transforms the spectrum of the frame and writes the PCM to `self.out`.
    fn imdct_output(&mut self, ftype: usize, wtype: usize) {
        let size = self.mtab.size;
        let old_pos = self.last_block_pos;

        for ch in 0..self.channels {
            self.imdct_and_window(ftype, wtype, ch, old_pos + 2 * ch * size);
        }

        let size2 = self.last_block_pos;
        let size1 = size - size2;

        for ch in 0..self.channels {
            let dst = &mut self.out[ch * size..(ch + 1) * size];
            let prev = &self.prev_frame[old_pos + 2 * ch * size..][..size1];
            dst[..size1].copy_from_slice(prev);
            dst[size1..].copy_from_slice(&self.curr_frame[2 * ch * size..][..size2]);
        }

        if self.channels == 2 {
            let (v1, v2) = self.out.split_at_mut(size);
            for (a, b) in v1.iter_mut().zip(v2.iter_mut()) {
                let t = *a - *b;
                *a += *b;
                *b = t;
            }
        }
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        if packet.data.len() < self.block_align {
            return decode_error("twinvq: packet too small");
        }

        self.read_bitstream(packet.data)?;

        let ftype = self.bits.ftype;
        let wtype = self.bits.window_type;

        self.read_and_decode_spectrum(ftype)?;
        self.imdct_output(ftype, wtype);
        std::mem::swap(&mut self.curr_frame, &mut self.prev_frame);

        let size = self.mtab.size;

        if self.discarded_packets < DISCARDED_PACKETS {
            self.discarded_packets += 1;
            return Ok(());
        }

        self.buf.clear();
        self.buf.render_uninit(Some(size));
        for ch in 0..self.channels {
            if let Some(plane) = self.buf.plane_mut(ch) {
                plane.copy_from_slice(&self.out[ch * size..(ch + 1) * size]);
            }
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for TwinVqDecoder {
    fn reset(&mut self) {
        self.reset_state();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        // The output of the first packets is dropped: an empty buffer is returned.
        self.buf.clear();

        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for TwinVqDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(TwinVqDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_TWINVQ, "twinvq", "VQF TwinVQ")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::ALL_MODES;
    use symphonia_core::audio::Audio;

    /// The (rate flag, kbit/s per channel) of the supported modes, in the order of `ALL_MODES`.
    const MODES: [(u32, u32); 9] =
        [(8, 8), (11, 8), (11, 10), (16, 16), (22, 20), (22, 24), (22, 32), (44, 40), (44, 48)];

    fn decoder(rate: u32, ibps: u32, channels: u32) -> Result<TwinVqDecoder> {
        let mut extra = Vec::new();
        for v in [channels - 1, ibps * channels, rate] {
            extra.extend_from_slice(&v.to_be_bytes());
        }
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_TWINVQ).with_extra_data(extra.into_boxed_slice());
        TwinVqDecoder::try_new(&p, &AudioDecoderOptions::default())
    }

    struct Rng(u64);

    impl Rng {
        fn fill(&mut self, data: &mut [u8]) {
            for b in data.iter_mut() {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                *b = (self.0 >> 32) as u8;
            }
        }
    }

    #[test]
    fn verify_mode_selection() {
        for (&(rate, ibps), mode) in MODES.iter().zip(ALL_MODES) {
            assert!(std::ptr::eq(select_mode(rate, ibps).unwrap(), mode));
        }
        assert!(select_mode(22, 48).is_none());
    }

    /// The bark band widths of every frame type must add up to the sub-block size, otherwise the
    /// envelope would not cover the spectrum.
    #[test]
    fn verify_bark_tables_cover_sub_blocks() {
        for mode in ALL_MODES {
            for fm in &mode.fmode {
                let covered: usize =
                    fm.bark_tab[..fm.bark_env_size].iter().map(|&w| usize::from(w)).sum();
                assert_eq!(covered, mode.size / fm.sub);
            }
        }
    }

    #[test]
    fn verify_invalid_parameters_are_rejected() {
        assert!(decoder(22, 48, 1).is_err());
        assert!(decoder(22, 20, 3).is_err());
        assert!(decoder(50, 20, 1).is_err());

        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_TWINVQ);
        assert!(TwinVqDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
        p.with_extra_data(Box::from([0u8; 11]));
        assert!(TwinVqDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }

    /// Arbitrary (corrupt) packets, with every window type, must never panic and must always
    /// yield finite output, in every mode.
    #[test]
    fn verify_random_packets_do_not_panic() {
        let mut rng = Rng(0x1234_5678_9abc_def0);

        for &(rate, ibps) in &MODES {
            for channels in [1, 2] {
                let mut dec = decoder(rate, ibps, channels).unwrap();
                let len = dec.block_align + 3;
                let mut data = vec![0u8; len];

                for n in 0..200 {
                    rng.fill(&mut data);
                    // Bytes 0 and 1 are the bit skip count and the previous byte; the window
                    // type nibble follows the skip. Valid values only in half of the packets.
                    data[0] = 8;
                    if n % 2 == 0 {
                        data[2] = (data[2] & 0x0f) | ((n as u8 / 2 % 9) << 4);
                    }
                    let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
                    if let Ok(GenericAudioBufferRef::F32(buf)) = dec.decode_ref(&pkt) {
                        for c in 0..buf.spec().channels().count() {
                            assert!(buf.plane(c).unwrap().iter().all(|s| s.is_finite()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn verify_short_packet_is_error() {
        let mut dec = decoder(22, 20, 1).unwrap();
        let data = vec![0u8; dec.block_align - 1];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    /// The first two packets only fill the overlap history and output nothing.
    #[test]
    fn verify_start_up_frames() {
        let mut dec = decoder(22, 20, 1).unwrap();
        let data = vec![0u8; dec.block_align];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);

        for n in 0..4 {
            let frames = match dec.decode_ref(&pkt).unwrap() {
                GenericAudioBufferRef::F32(b) => b.frames(),
                _ => panic!(),
            };
            assert_eq!(frames, if n < 2 { 0 } else { 1024 });
        }
    }
}
