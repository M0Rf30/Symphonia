// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Intel Music Coder (IMC) and Indeo Audio Coder (IAC) decoder.
//!
//! IMC is a proprietary MDCT codec: a 256-coefficient transform split into 32 bands, with
//! scale factors that are Huffman coded (or raw) and a per-band bit allocation that the decoder
//! re-derives from them. There is no public specification; this decoder follows the structure and
//! arithmetic of the FFmpeg `imc` and `iac` decoders (`libavcodec/imc.c`, see `NOTICE`).
//!
//! Each 64-byte block holds 256 samples of one channel. IMC (`WAVE_FORMAT` 0x0401) is mono, IAC
//! (0x0402) may be stereo, in which case a packet frame is two consecutive blocks that carry the
//! sum and the difference of the channels. A packet holds one or more whole frames; a trailing
//! partial frame is ignored. The sample rate only matters for IAC, which derives its masking
//! tables from it.

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
// The decoder mirrors the reference implementation's index-based loops.
#![allow(clippy::needless_range_loop)]

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_IAC, CODEC_ID_IMC};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    BAND_TAB, CB_SELECT, CYCL_TAB, CYCL_TAB2, EXP_TAB, HUFFMAN_LENS, HUFFMAN_SIZES, HUFFMAN_SYMS,
    QUANTIZER1, QUANTIZER2, WEIGHTS1, WEIGHTS2, X_TAB,
};

/// Size of a block (one channel) in bytes.
const IMC_BLOCK_SIZE: usize = 64;
/// Number of bits in a block.
const IMC_BLOCK_BITS: i32 = 512;
const BANDS: usize = 32;
/// Number of MDCT coefficients, and output samples, per block.
const COEFFS: usize = 256;
/// Half the number of coefficients: the overlap length.
const HALF: usize = COEFFS / 2;
/// Maximum number of bits of a Huffman code.
const MAX_CODE_BITS: u32 = 18;

/// Number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 16;

const BIG: f32 = 1.0e20;

/// `log2(10) * 0.25`.
const LOG_STEP: f64 = 0.83048;

/// Reads bits MSB first from a 64-byte block whose 16-bit words have been byte-swapped (the
/// stream stores little-endian words). Reads past the end return zeros.
struct Bits {
    data: [u8; IMC_BLOCK_SIZE],
    pos: usize,
}

impl Bits {
    fn new(block: &[u8]) -> Self {
        let mut data = [0u8; IMC_BLOCK_SIZE];
        for (d, s) in data.chunks_exact_mut(2).zip(block.chunks_exact(2)) {
            d[0] = s[1];
            d[1] = s[0];
        }
        Bits { data, pos: 0 }
    }

    /// Returns the next `n` (at most 24) bits without consuming them.
    fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let mut v = 0u32;
        for k in 0..4 {
            v = (v << 8) | u32::from(self.data.get(byte + k).copied().unwrap_or(0));
        }
        (v << (self.pos & 7)) >> (32 - n)
    }

    fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    fn get1(&mut self) -> u32 {
        self.get(1)
    }
}

/// A Huffman code.
#[derive(Copy, Clone, Default)]
struct Code {
    code: u32,
    len: u8,
    sym: u8,
}

/// Canonical Huffman table. Codes are assigned in table order, as FFmpeg's
/// `ff_vlc_init_from_lengths` does.
#[derive(Clone)]
struct Huffman {
    codes: [Code; 18],
    count: usize,
}

impl Huffman {
    fn new(lens: &[u8; 18], syms: &[u8; 18], count: usize) -> Self {
        let mut codes = [Code::default(); 18];
        let mut n = 0;
        let mut code = 0u64;
        for (&len, &sym) in lens.iter().zip(syms.iter()).take(count) {
            if len == 0 {
                continue;
            }
            codes[n] = Code { code: (code >> (32 - u32::from(len))) as u32, len, sym };
            n += 1;
            code += 1u64 << (32 - u32::from(len));
        }
        Huffman { codes, count: n }
    }

    fn decode(&self, bs: &mut Bits) -> Result<i32> {
        let peek = bs.peek(MAX_CODE_BITS);
        for c in &self.codes[..self.count] {
            if peek >> (MAX_CODE_BITS - u32::from(c.len)) == c.code {
                bs.pos += usize::from(c.len);
                return Ok(i32::from(c.sym));
            }
        }
        decode_error("imc: invalid huffman code")
    }
}

fn freq2bark(freq: f64) -> f64 {
    3.5 * ((freq / 7500.0) * (freq / 7500.0)).atan() + 13.0 * (freq * 0.00076).atan()
}

/// Masking tables. For IMC they are fixed; for IAC they are derived from the sample rate
/// (`iac_generate_tabs`).
struct Masking {
    cycl: [i32; BANDS],
    cycl2: [i32; BANDS],
    weights1: [f32; BANDS - 1],
    weights2: [f32; BANDS - 1],
}

impl Masking {
    fn imc() -> Self {
        let mut cycl = [0i32; BANDS];
        let mut cycl2 = [0i32; BANDS];
        for i in 0..BANDS {
            cycl[i] = i32::from(CYCL_TAB[i]);
            cycl2[i] = i32::from(CYCL_TAB2[i]);
        }
        Masking { cycl, cycl2, weights1: WEIGHTS1, weights2: WEIGHTS2 }
    }

    fn iac(sample_rate: u32) -> Self {
        let rate = f64::from(sample_rate);
        let scale = rate / (256.0 * 2.0 * 2.0);
        let nyquist = rate * 0.5;

        let mut freqmin = [0f64; BANDS];
        let mut freqmid = [0f64; BANDS];
        let mut freqmax = [0f64; BANDS];
        let mut m = Masking::imc();
        let mut prev_bark = 0.0;

        for i in 0..BANDS {
            let freq = f64::from(BAND_TAB[i] + BAND_TAB[i + 1] - 1) * scale;
            let bark = freq2bark(freq);

            if i > 0 {
                let tb = bark - prev_bark;
                m.weights1[i - 1] = 10f64.powf(-tb) as f32;
                m.weights2[i - 1] = 10f64.powf(-2.7 * tb) as f32;
            }
            prev_bark = bark;
            freqmid[i] = freq;

            let mut tf = freq;
            while tf < nyquist {
                tf += 0.5;
                if freq2bark(tf) > bark + 0.5 {
                    break;
                }
            }
            freqmax[i] = tf;

            tf = freq;
            while tf > 0.0 {
                tf -= 0.5;
                if freq2bark(tf) <= bark - 0.5 {
                    break;
                }
            }
            freqmin[i] = tf;
        }

        for i in 0..BANDS {
            let mut j = BANDS - 1;
            while j > 0 && freqmax[i] <= freqmid[j] {
                j -= 1;
            }
            m.cycl[i] = (j + 1) as i32;

            let mut j = 0;
            while j < BANDS && freqmin[i] >= freqmid[j] {
                j += 1;
            }
            m.cycl2[i] = j as i32 - 1;
        }

        // The loops above guarantee `i < cycl <= 32` and `-1 <= cycl2 < i`; enforce it so that the
        // later band loops are always in range.
        for i in 0..BANDS {
            m.cycl[i] = m.cycl[i].clamp(i as i32 + 1, BANDS as i32);
            m.cycl2[i] = m.cycl2[i].clamp(-1, i as i32 - 1);
        }
        m
    }
}

/// Per-channel decoder state, mirroring FFmpeg's `IMCChannel`.
struct Channel {
    old_floor: [f32; BANDS],
    flcoeffs1: [f32; BANDS],
    flcoeffs2: [f32; BANDS],
    flcoeffs3: [f32; BANDS],
    flcoeffs4: [f32; BANDS],
    flcoeffs5: [f32; BANDS],
    flcoeffs6: [f32; BANDS],
    cw_decoded: [f32; COEFFS],

    /// Codewords per band.
    band_width: [i32; BANDS],
    /// Bits per codeword in a band.
    bits_band: [i32; BANDS],
    /// Bits in each codeword.
    cw_length: [i32; COEFFS],
    levl_coeff: [i32; BANDS],
    band_flags: [i32; BANDS],
    /// Bits for all coefficients in a band.
    sum_len: [i32; BANDS],
    /// Skip flags are stored in raw form.
    skip_flag_raw: [bool; BANDS],
    /// Bits used to code the skip flags.
    skip_flag_bits: [i32; BANDS],
    /// Skipped coefficients per band.
    skip_flag_count: [i32; BANDS],
    skip_flags: [bool; COEFFS],
    codewords: [i32; COEFFS],

    decoder_reset: bool,
    prev_win: [f32; HALF],
}

impl Channel {
    fn new() -> Self {
        Channel {
            old_floor: [1.0; BANDS],
            flcoeffs1: [0.0; BANDS],
            flcoeffs2: [0.0; BANDS],
            flcoeffs3: [0.0; BANDS],
            flcoeffs4: [0.0; BANDS],
            flcoeffs5: [0.0; BANDS],
            flcoeffs6: [0.0; BANDS],
            cw_decoded: [0.0; COEFFS],
            band_width: [0; BANDS],
            bits_band: [0; BANDS],
            cw_length: [0; COEFFS],
            levl_coeff: [0; BANDS],
            band_flags: [0; BANDS],
            sum_len: [0; BANDS],
            skip_flag_raw: [false; BANDS],
            skip_flag_bits: [0; BANDS],
            skip_flag_count: [0; BANDS],
            skip_flags: [false; COEFFS],
            codewords: [0; COEFFS],
            decoder_reset: true,
            prev_win: [0.0; HALF],
        }
    }
}

/// Number of coefficients in band `i`.
fn band_size(i: usize) -> i32 {
    i32::from(BAND_TAB[i + 1] - BAND_TAB[i])
}

/// The coefficient range of band `i`.
fn band_range(i: usize) -> std::ops::Range<usize> {
    usize::from(BAND_TAB[i])..usize::from(BAND_TAB[i + 1])
}

/// Index of the first band that is bit-allocated.
fn first_band(fmt: u32) -> usize {
    if fmt & 2 != 0 { 4 } else { 0 }
}

/// Shared decoder state.
struct Core {
    iac: bool,
    channels: usize,
    masking: Masking,
    huffman: [[Huffman; 4]; 4],
    imdct: Imdct,
    /// `sqrt(2)`-scaled sine window.
    window: [f32; COEFFS],
    sqrt_tab: [f32; 30],
    /// Position of the first scale factor in raw level coefficients.
    coef0_pos: usize,
    /// Full IMDCT output.
    full: Box<[f32; 2 * COEFFS]>,
    temp: [f32; COEFFS],
}

impl Core {
    fn new(iac: bool, channels: usize, sample_rate: u32) -> Self {
        let mut window = [0f32; COEFFS];
        for (i, w) in window.iter_mut().enumerate() {
            // `ff_sine_window_init`, normalized with sqrt(2).
            let s = ((i as f64 + 0.5) * (std::f64::consts::PI / (2.0 * COEFFS as f64))).sin();
            *w = (s as f32) * std::f64::consts::SQRT_2 as f32;
        }

        let mut sqrt_tab = [0f32; 30];
        for (i, s) in sqrt_tab.iter_mut().enumerate() {
            *s = (i as f64).sqrt() as f32;
        }

        let huffman = std::array::from_fn(|i| {
            std::array::from_fn(|j| {
                Huffman::new(&HUFFMAN_LENS[i][j], &HUFFMAN_SYMS[i][j], HUFFMAN_SIZES[i])
            })
        });

        Core {
            iac,
            channels,
            masking: if iac { Masking::iac(sample_rate) } else { Masking::imc() },
            huffman,
            imdct: Imdct::new_scaled(COEFFS, 1.0 / 16384.0),
            window,
            sqrt_tab,
            coef0_pos: 0,
            full: Box::new([0.0; 2 * COEFFS]),
            temp: [0.0; COEFFS],
        }
    }

    fn read_level_coeffs(&self, bs: &mut Bits, fmt: u32, levl: &mut [i32; BANDS]) -> Result<()> {
        let s = (fmt >> 1) as usize;
        let cb_sel = &CB_SELECT[s];
        let mut start = 0;

        if fmt & 4 != 0 {
            start = 1;
            levl[0] = bs.get(7) as i32;
        }
        for i in start..BANDS {
            levl[i] = self.huffman[s][usize::from(cb_sel[i])].decode(bs)?;
            if levl[i] == 17 {
                levl[i] += bs.get(4) as i32;
            }
        }
        Ok(())
    }

    fn read_level_coeffs_raw(&mut self, bs: &mut Bits, levl: &mut [i32; BANDS]) {
        self.coef0_pos = bs.get(5) as usize;
        levl[0] = bs.get(7) as i32;
        for l in levl[1..].iter_mut() {
            *l = bs.get(4) as i32;
        }
    }

    fn decode_level_coefficients(ch: &mut Channel) -> Result<()> {
        // `0.18945 = log2(10) * 0.05703125`.
        ch.flcoeffs1[0] = (20000.0 / (f64::from(ch.levl_coeff[0]) * 0.18945).exp2()) as f32;
        ch.flcoeffs2[0] = ch.flcoeffs1[0].log2();
        let mut tmp = ch.flcoeffs1[0];
        let mut tmp2 = ch.flcoeffs2[0];

        for i in 1..BANDS {
            let mut level = ch.levl_coeff[i];
            if level == 16 {
                ch.flcoeffs1[i] = 1.0;
                ch.flcoeffs2[i] = 0.0;
            }
            else {
                if level < 17 {
                    level -= 7;
                }
                else if level <= 24 {
                    level -= 32;
                }
                else {
                    level -= 16;
                }

                let Some(&e) = EXP_TAB.get((15 + level) as usize)
                else {
                    return decode_error("imc: level coefficient out of range");
                };
                tmp *= e;
                tmp2 = (f64::from(tmp2) + LOG_STEP * f64::from(level)) as f32;
                ch.flcoeffs1[i] = tmp;
                ch.flcoeffs2[i] = tmp2;
            }
        }
        Ok(())
    }

    fn decode_level_coefficients2(ch: &mut Channel) -> Result<()> {
        for i in 0..BANDS {
            ch.flcoeffs1[i] = 0.0;
            let level = ch.levl_coeff[i];
            if level < 16 {
                // `imc_exp_tab2 = imc_exp_tab + 8`.
                let Some(&e) = EXP_TAB.get((level + 8) as usize)
                else {
                    return decode_error("imc: level coefficient out of range");
                };
                ch.flcoeffs1[i] = e * ch.old_floor[i];
                ch.flcoeffs2[i] =
                    (f64::from(level - 7) * LOG_STEP + f64::from(ch.flcoeffs2[i])) as f32;
            }
            else {
                ch.flcoeffs1[i] = ch.old_floor[i];
            }
        }
        Ok(())
    }

    fn decode_level_coefficients_raw(coef0_pos: usize, ch: &mut Channel) {
        let pos = coef0_pos;
        ch.flcoeffs1[pos] = (20000.0 / (f64::from(ch.levl_coeff[0]) * 0.18945).exp2()) as f32;
        ch.flcoeffs2[pos] = ch.flcoeffs1[pos].log2();
        let tmp = ch.flcoeffs1[pos];
        let tmp2 = ch.flcoeffs2[pos];

        let mut k = 1;
        for i in 0..BANDS {
            if i == pos {
                continue;
            }
            let level = ch.levl_coeff[k];
            k += 1;
            ch.flcoeffs1[i] = tmp * 10f32.powf((-f64::from(level) * 0.4375) as f32);
            // `1.4533435415 = log2(10) * 0.4375`.
            ch.flcoeffs2[i] = (f64::from(tmp2) - 1.4533435415 * f64::from(level)) as f32;
        }
    }

    fn calculate_coeffs(&self, ch: &mut Channel) {
        let mut work_t1 = [0f32; BANDS];
        let mut work_t2 = [0f32; BANDS];
        let mut work_t3 = [0f32; BANDS];
        let snr_limit = 1.0e-30f32;
        let m = &self.masking;

        for i in 0..BANDS {
            ch.flcoeffs5[i] = 0.0;
            work_t2[i] = 0.0;
            if ch.band_width[i] != 0 {
                work_t1[i] = ch.flcoeffs1[i] * ch.flcoeffs1[i];
                ch.flcoeffs3[i] = 2.0 * ch.flcoeffs2[i];
            }
            else {
                work_t1[i] = 0.0;
                ch.flcoeffs3[i] = -30000.0;
            }
            work_t3[i] = ((ch.band_width[i] as f32 * work_t1[i]) as f64 * 0.01) as f32;
            if work_t3[i] <= snr_limit {
                work_t3[i] = 0.0;
            }
        }

        for i in 0..BANDS {
            let mut cnt2 = i as i32;
            while cnt2 < m.cycl[i] {
                ch.flcoeffs5[cnt2 as usize] += work_t3[i];
                cnt2 += 1;
            }
            work_t2[(cnt2 - 1) as usize] += work_t3[i];
        }

        let mut accum = 0f32;
        for i in 1..BANDS {
            accum = (work_t2[i - 1] + accum) * m.weights1[i - 1];
            ch.flcoeffs5[i] += accum;
        }

        work_t2 = [0.0; BANDS];

        for i in 0..BANDS {
            let mut cnt2 = i as i32 - 1;
            while cnt2 > m.cycl2[i] {
                ch.flcoeffs5[cnt2 as usize] += work_t3[i];
                cnt2 -= 1;
            }
            work_t2[(cnt2 + 1) as usize] += work_t3[i];
        }

        accum = 0.0;
        for i in (0..=BANDS - 2).rev() {
            accum = (work_t2[i + 1] + accum) * m.weights2[i];
            ch.flcoeffs5[i] += accum;
        }
    }

    /// Performs the bit allocation depending on the bits available.
    fn bit_allocation(ch: &mut Channel, fmt: u32, freebits: i32, flag: usize) -> Result<()> {
        let limit = -1.0e20f32;
        let mut highest = 0f32;
        let mut t1 = 0i32;
        let mut t2 = 1i32;
        let mut summa = 0f32;
        let mut iacc = 0i32;
        let mut summer = 0i32;
        let first = first_band(fmt);

        for i in 0..BANDS {
            highest = highest.max(ch.flcoeffs1[i]);
        }

        for i in 0..BANDS - 1 {
            if ch.flcoeffs5[i] <= 0.0 {
                return decode_error("imc: invalid flcoeffs5");
            }
            ch.flcoeffs4[i] = ch.flcoeffs3[i] - ch.flcoeffs5[i].log2();
        }
        ch.flcoeffs4[BANDS - 1] = limit;

        highest = (f64::from(highest) * 0.25) as f32;

        for i in 0..BANDS {
            let size = band_size(i);
            let bw = ch.band_width[i];
            let mut indx = -1i32;

            if size == bw {
                indx = 0;
            }
            if size > bw {
                indx = 1;
            }
            if size / 2 >= bw {
                indx = 2;
            }
            if indx == -1 {
                return decode_error("imc: invalid band width");
            }

            let low = usize::from(ch.flcoeffs1[i] < highest);
            ch.flcoeffs4[i] += X_TAB[(indx as usize * 2 + low) * 2 + flag];
        }

        if fmt & 0x2 != 0 {
            ch.flcoeffs4[..4].fill(limit);
        }

        for i in first..BANDS - 1 {
            iacc += ch.band_width[i];
            summa += ch.band_width[i] as f32 * ch.flcoeffs4[i];
        }

        if iacc == 0 {
            return decode_error("imc: empty bit allocation");
        }

        ch.band_width[BANDS - 1] = 0;
        summa = ((f64::from(summa) * 0.5 - f64::from(freebits)) / f64::from(iacc)) as f32;

        for i in 0..BANDS / 2 {
            let rres = summer - freebits;
            if (-8..=8).contains(&rres) {
                break;
            }

            summer = 0;
            iacc = 0;

            for j in first..BANDS {
                let v = ((f64::from(ch.flcoeffs4[j]) * 0.5) - f64::from(summa) + 0.5) as f32;
                let cwlen = v.clamp(0.0, 6.0) as i32;

                ch.bits_band[j] = cwlen;
                summer += ch.band_width[j] * cwlen;

                if cwlen > 0 {
                    iacc += ch.band_width[j];
                }
            }

            let mut flg = t2;
            t2 = 1;
            if freebits < summer {
                t2 = -1;
            }
            if i == 0 {
                flg = t2;
            }
            if flg != t2 {
                t1 += 1;
            }

            summa += (summer - freebits) as f32 / ((t1 + 1) * iacc) as f32;
        }

        for i in first..BANDS {
            for j in band_range(i) {
                ch.cw_length[j] = ch.bits_band[i];
            }
        }

        let mut work_t = [0f32; BANDS];

        if freebits > summer {
            for i in 0..BANDS {
                work_t[i] = if ch.bits_band[i] == 6 {
                    -BIG
                }
                else {
                    (f64::from((ch.bits_band[i] * -2) as f32 + ch.flcoeffs4[i]) - 0.415) as f32
                };
            }

            let mut highest = 0f32;

            loop {
                if f64::from(highest) <= -1.0e20 {
                    break;
                }

                let mut found_indx = 0;
                highest = -BIG;

                for i in 0..BANDS {
                    if work_t[i] > highest {
                        highest = work_t[i];
                        found_indx = i;
                    }
                }

                if f64::from(highest) > -1.0e20 {
                    work_t[found_indx] = (f64::from(work_t[found_indx]) - 2.0) as f32;
                    ch.bits_band[found_indx] += 1;
                    if ch.bits_band[found_indx] == 6 {
                        work_t[found_indx] = -BIG;
                    }

                    for j in band_range(found_indx) {
                        if freebits <= summer {
                            break;
                        }
                        ch.cw_length[j] += 1;
                        summer += 1;
                    }
                }

                if freebits <= summer {
                    break;
                }
            }
        }

        if freebits < summer {
            for i in 0..BANDS {
                work_t[i] = if ch.bits_band[i] != 0 {
                    (f64::from((ch.bits_band[i] * -2) as f32 + ch.flcoeffs4[i]) + 1.585) as f32
                }
                else {
                    BIG
                };
            }
            if fmt & 0x2 != 0 {
                work_t[..4].fill(BIG);
            }

            while freebits < summer {
                let mut lowest = 1.0e10f32;
                let mut low_indx = 0;
                for i in 0..BANDS {
                    if work_t[i] < lowest {
                        lowest = work_t[i];
                        low_indx = i;
                    }
                }
                // Nothing can be reduced any more: the stream asks for fewer bits than the
                // minimum allocation. The reference decoder would loop forever here.
                if lowest >= 1.0e10 {
                    return decode_error("imc: bit allocation does not converge");
                }
                work_t[low_indx] = (f64::from(lowest) + 2.0) as f32;

                ch.bits_band[low_indx] -= 1;
                if ch.bits_band[low_indx] == 0 {
                    work_t[low_indx] = BIG;
                }

                for j in band_range(low_indx) {
                    if freebits >= summer {
                        break;
                    }
                    if ch.cw_length[j] > 0 {
                        ch.cw_length[j] -= 1;
                        summer -= 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn get_skip_coeff(bs: &mut Bits, ch: &mut Channel) {
        ch.skip_flag_bits = [0; BANDS];
        ch.skip_flag_count = [0; BANDS];

        for i in 0..BANDS {
            if ch.band_flags[i] == 0 || ch.band_width[i] == 0 {
                continue;
            }

            let range = band_range(i);
            if !ch.skip_flag_raw[i] {
                ch.skip_flag_bits[i] = band_size(i);

                for j in range {
                    ch.skip_flags[j] = bs.get1() != 0;
                    if ch.skip_flags[j] {
                        ch.skip_flag_count[i] += 1;
                    }
                }
            }
            else {
                let mut j = range.start;
                while j + 1 < range.end {
                    if bs.get1() == 0 {
                        // 0
                        ch.skip_flag_bits[i] += 1;
                        ch.skip_flags[j] = true;
                        ch.skip_flags[j + 1] = true;
                        ch.skip_flag_count[i] += 2;
                    }
                    else if bs.get1() != 0 {
                        // 11
                        ch.skip_flag_bits[i] += 2;
                        ch.skip_flags[j] = false;
                        ch.skip_flags[j + 1] = true;
                        ch.skip_flag_count[i] += 1;
                    }
                    else {
                        ch.skip_flag_bits[i] += 3;
                        ch.skip_flags[j + 1] = false;
                        if bs.get1() == 0 {
                            // 100
                            ch.skip_flags[j] = true;
                            ch.skip_flag_count[i] += 1;
                        }
                        else {
                            // 101
                            ch.skip_flags[j] = false;
                        }
                    }
                    j += 2;
                }

                if j < range.end {
                    ch.skip_flag_bits[i] += 1;
                    ch.skip_flags[j] = bs.get1() != 0;
                    if ch.skip_flags[j] {
                        ch.skip_flag_count[i] += 1;
                    }
                }
            }
        }
    }

    /// Increases the highest bands' coefficient sizes as some bits won't be used.
    fn adjust_bit_allocation(ch: &mut Channel, summer: i32) {
        let mut work_t = [0f32; BANDS];
        let mut corrected = 0;
        let mut highest = 0f32;
        let mut found_indx = 0;

        for i in 0..BANDS {
            work_t[i] = if ch.bits_band[i] == 6 {
                -BIG
            }
            else {
                (f64::from((ch.bits_band[i] * -2) as f32 + ch.flcoeffs4[i]) - 0.415) as f32
            };
        }

        while corrected < summer {
            if f64::from(highest) <= -1.0e20 {
                break;
            }

            highest = -BIG;

            for i in 0..BANDS {
                if work_t[i] > highest {
                    highest = work_t[i];
                    found_indx = i;
                }
            }

            if f64::from(highest) > -1.0e20 {
                work_t[found_indx] = (f64::from(work_t[found_indx]) - 2.0) as f32;
                ch.bits_band[found_indx] += 1;
                if ch.bits_band[found_indx] == 6 {
                    work_t[found_indx] = -BIG;
                }

                for j in band_range(found_indx) {
                    if corrected >= summer {
                        break;
                    }
                    if !ch.skip_flags[j] && ch.cw_length[j] < 6 {
                        ch.cw_length[j] += 1;
                        corrected += 1;
                    }
                }
            }
        }
    }

    fn inverse_quant_coeff(ch: &mut Channel, fmt: u32) -> Result<()> {
        for i in 0..BANDS {
            for j in band_range(i) {
                ch.cw_decoded[j] = 0.0;
                let cw_len = ch.cw_length[j];

                if cw_len <= 0 || ch.skip_flags[j] {
                    continue;
                }

                // The reference decoder never produces more than 6 bit codewords, and its
                // quantizer tables end there.
                if cw_len > 6 {
                    return decode_error("imc: codeword too long");
                }

                let max_size = 1i32 << cw_len;
                let middle = max_size >> 1;
                let cw = ch.codewords[j];

                if cw >= max_size || cw < 0 {
                    return decode_error("imc: invalid codeword");
                }

                let scale = ch.flcoeffs6[i];
                let v = if cw_len >= 4 {
                    let q = &QUANTIZER2[((fmt & 2) >> 1) as usize];
                    if cw >= middle {
                        q[(cw - 8) as usize] * scale
                    }
                    else {
                        -q[(max_size - cw - 8 - 1) as usize] * scale
                    }
                }
                else {
                    let q =
                        &QUANTIZER1[(((fmt & 2) >> 1) | ((ch.band_flags[i] as u32) << 1)) as usize];
                    if cw >= middle {
                        q[(cw - 1) as usize] * scale
                    }
                    else {
                        -q[(max_size - 2 - cw) as usize] * scale
                    }
                };
                ch.cw_decoded[j] = v;
            }
        }
        Ok(())
    }

    fn get_coeffs(bs: &mut Bits, ch: &mut Channel) {
        for i in 0..BANDS {
            if ch.sum_len[i] == 0 {
                continue;
            }
            if ch.band_flags[i] != 0 || ch.band_width[i] != 0 {
                for j in band_range(i) {
                    let cw_len = ch.cw_length[j];
                    let mut cw = 0;

                    // A codeword that would run off the block is a "potential problem" in the
                    // reference decoder, which reads a zero codeword instead.
                    if cw_len > 0
                        && (ch.band_flags[i] == 0 || !ch.skip_flags[j])
                        && cw_len <= 24
                        && bs.pos as i32 + cw_len <= IMC_BLOCK_BITS
                    {
                        cw = bs.get(cw_len as u32) as i32;
                    }

                    ch.codewords[j] = cw;
                }
            }
        }
    }

    fn refine_bit_allocation(&self, bs: &mut Bits, ch: &mut Channel) {
        for i in 0..BANDS {
            ch.sum_len[i] = 0;
            ch.skip_flag_raw[i] = false;
            for j in band_range(i) {
                ch.sum_len[i] += ch.cw_length[j];
            }
            if ch.band_flags[i] != 0
                && (f64::from(band_size(i)) * 1.5) as i32 > ch.sum_len[i]
                && ch.sum_len[i] > 0
            {
                ch.skip_flag_raw[i] = true;
            }
        }

        Self::get_skip_coeff(bs, ch);

        for i in 0..BANDS {
            ch.flcoeffs6[i] = ch.flcoeffs1[i];
            // Band has the flag set and at least one coded coefficient.
            if ch.band_flags[i] != 0 && band_size(i) != ch.skip_flag_count[i] {
                let size = band_size(i) as usize;
                let coded = (band_size(i) - ch.skip_flag_count[i]) as usize;
                ch.flcoeffs6[i] *= self.sqrt_tab[size] / self.sqrt_tab[coded];
            }
        }

        // Calculate the bits left and needed, and adjust the bit allocation.
        let mut summer = 0;

        for i in 0..BANDS {
            if ch.band_flags[i] != 0 {
                for j in band_range(i) {
                    if ch.skip_flags[j] {
                        summer += ch.cw_length[j];
                        ch.cw_length[j] = 0;
                    }
                }
                summer -= ch.skip_flag_bits[i];
            }
        }
        Self::adjust_bit_allocation(ch, summer);
    }

    /// Decodes the block of channel `ch` and writes 256 samples to `out`.
    fn decode_block(&mut self, ch: &mut Channel, block: &[u8], out: &mut [f32]) -> Result<()> {
        let mut bs = Bits::new(block);
        let r = self.decode_channel(&mut bs, ch, out);
        if r.is_err() {
            ch.skip_flags = [false; COEFFS];
        }
        r
    }

    fn decode_channel(&mut self, bs: &mut Bits, ch: &mut Channel, out: &mut [f32]) -> Result<()> {
        // Check the frame header.
        let hdr = bs.get(9);
        if hdr & 0x18 != 0 {
            return decode_error("imc: frame header check failed");
        }
        let fmt = bs.get(3);

        if fmt & 0x4 != 0 {
            ch.decoder_reset = true;
        }

        if ch.decoder_reset {
            ch.old_floor = [1.0; BANDS];
            ch.cw_decoded = [0.0; COEFFS];
            ch.decoder_reset = false;
        }

        let flag = bs.get1() as usize;
        let mut levl = ch.levl_coeff;
        if fmt & 0x1 != 0 {
            self.read_level_coeffs_raw(bs, &mut levl);
        }
        else {
            self.read_level_coeffs(bs, fmt, &mut levl)?;
        }
        ch.levl_coeff = levl;

        if fmt & 0x1 != 0 {
            if self.coef0_pos >= BANDS {
                return decode_error("imc: invalid first coefficient position");
            }
            Self::decode_level_coefficients_raw(self.coef0_pos, ch);
        }
        else if fmt & 0x4 != 0 {
            Self::decode_level_coefficients(ch)?;
        }
        else {
            Self::decode_level_coefficients2(ch)?;
        }

        for i in 0..BANDS {
            if ch.flcoeffs1[i] > i32::MAX as f32 {
                return decode_error("imc: scalefactor out of range");
            }
        }

        ch.old_floor = ch.flcoeffs1;

        if fmt & 0x1 != 0 {
            for i in 0..BANDS {
                ch.band_width[i] = band_size(i);
                ch.band_flags[i] = 0;
                ch.flcoeffs3[i] = ch.flcoeffs2[i] * 2.0;
                ch.flcoeffs5[i] = 1.0;
            }
        }
        else {
            for i in 0..BANDS {
                ch.band_width[i] = if ch.levl_coeff[i] == 16 { 0 } else { band_size(i) };
            }

            ch.band_flags = [0; BANDS];
            for i in 0..BANDS - 1 {
                if ch.band_width[i] != 0 {
                    ch.band_flags[i] = bs.get1() as i32;
                }
            }

            self.calculate_coeffs(ch);
        }

        let mut bitscount = 0;
        // The first 4 bands are assigned 5 bits per coefficient.
        if fmt & 0x2 != 0 {
            bitscount += 15;

            ch.bits_band[0] = 5;
            ch.cw_length[0] = 5;
            ch.cw_length[1] = 5;
            ch.cw_length[2] = 5;
            for i in 1..4 {
                let bits = if fmt & 0x1 != 0 {
                    5
                }
                else if ch.levl_coeff[i] == 16 {
                    0
                }
                else {
                    5
                };
                ch.bits_band[i] = bits;
                for j in band_range(i) {
                    ch.cw_length[j] = bits;
                    bitscount += bits;
                }
            }
        }
        if self.iac {
            bitscount += i32::from(ch.band_width[BANDS - 1] != 0);
            if fmt & 0x2 == 0 {
                bitscount += 16;
            }
        }

        let freebits = IMC_BLOCK_BITS - bitscount - bs.pos as i32;
        if let Err(e) = Self::bit_allocation(ch, fmt, freebits, flag) {
            ch.decoder_reset = true;
            return Err(e);
        }

        if fmt & 0x1 != 0 {
            ch.skip_flags[..BANDS].fill(false);
        }
        else {
            self.refine_bit_allocation(bs, ch);
        }

        for i in 0..BANDS {
            ch.sum_len[i] = 0;

            for j in band_range(i) {
                if !ch.skip_flags[j] {
                    ch.sum_len[i] += ch.cw_length[j];
                }
            }
        }

        ch.codewords = [0; COEFFS];

        Self::get_coeffs(bs, ch);

        if let Err(e) = Self::inverse_quant_coeff(ch, fmt) {
            ch.decoder_reset = true;
            return Err(e);
        }

        ch.skip_flags = [false; COEFFS];

        // The reference decoder uses the first half of the IMDCT output, which starts a quarter of
        // the way into the full 2N-sample output.
        // Symphonia's IMDCT has the opposite sign to FFmpeg's `av_tx` inverse MDCT.
        self.imdct.imdct(&ch.cw_decoded, &mut self.full[..]);
        for (t, &f) in self.temp.iter_mut().zip(&self.full[HALF..HALF + COEFFS]) {
            *t = -f;
        }

        // `vector_fmul_window`: overlap-add with the previous block.
        for k in 0..HALF {
            let s0 = ch.prev_win[k];
            let s1 = self.temp[HALF - 1 - k];
            let wi = self.window[k];
            let wj = self.window[COEFFS - 1 - k];
            out[k] = s0 * wj - s1 * wi;
            out[COEFFS - 1 - k] = s0 * wi + s1 * wj;
        }
        ch.prev_win.copy_from_slice(&self.temp[HALF..]);

        Ok(())
    }
}

/// Intel Music Coder / Indeo Audio Coder decoder.
pub struct ImcDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    chan: [Channel; 2],
    core: Core,
}

impl ImcDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let iac = match params.codec {
            CODEC_ID_IMC => false,
            CODEC_ID_IAC => true,
            _ => return unsupported_error("imc: invalid codec"),
        };

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("imc: sample rate is required"),
        };

        // As in the reference decoder, an IAC stream with an absurd rate would need new
        // tables.
        if iac && rate > 96000 {
            return unsupported_error("imc: unsupported sample rate");
        }

        // IMC is always mono, whatever the container claims. IAC follows the container.
        let num_channels = if iac {
            match params.channels.as_ref().map(|c| c.count()) {
                Some(n @ (1 | 2)) => n,
                Some(_) => return unsupported_error("imc: more than 2 channels"),
                None => return unsupported_error("imc: channel layout is required"),
            }
        }
        else {
            1
        };

        let layout = if num_channels == 2 {
            layouts::CHANNEL_LAYOUT_STEREO
        }
        else {
            layouts::CHANNEL_LAYOUT_MONO
        };

        let buf = AudioBuffer::new(AudioSpec::new(rate, layout.clone()), INITIAL_FRAMES * COEFFS);

        let mut params = params.clone();
        params.with_channels(layout);

        Ok(ImcDecoder {
            params,
            opts: *opts,
            buf,
            chan: [Channel::new(), Channel::new()],
            core: Core::new(iac, num_channels, rate),
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let channels = self.core.channels;
        let frame_len = IMC_BLOCK_SIZE * channels;
        let frames = packet.data.len() / frame_len;

        if frames == 0 {
            return decode_error("imc: packet is too small");
        }

        if frames * COEFFS > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames * COEFFS);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames * COEFFS));

        if channels == 1 {
            let out = self.buf.plane_mut(0).expect("mono plane");
            for (block, audio) in
                packet.data.chunks_exact(frame_len).zip(out.chunks_exact_mut(COEFFS))
            {
                self.core.decode_block(&mut self.chan[0], block, audio)?;
            }
        }
        else {
            let (left, right) = self.buf.plane_pair_mut(0, 1).expect("stereo planes");
            for ((frame, l), r) in packet
                .data
                .chunks_exact(frame_len)
                .zip(left.chunks_exact_mut(COEFFS))
                .zip(right.chunks_exact_mut(COEFFS))
            {
                self.core.decode_block(&mut self.chan[0], &frame[..IMC_BLOCK_SIZE], l)?;
                self.core.decode_block(&mut self.chan[1], &frame[IMC_BLOCK_SIZE..], r)?;

                // `butterflies_float`: the channels carry the sum and the difference.
                for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                    let t = *a - *b;
                    *a += *b;
                    *b = t;
                }
            }
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for ImcDecoder {
    fn reset(&mut self) {
        for c in self.chan.iter_mut() {
            c.decoder_reset = true;
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        let supported = Self::supported_codecs();
        let idx = usize::from(self.core.iac);
        &supported[idx].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
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

impl RegisterableAudioDecoder for ImcDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(ImcDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_IMC, "imc", "IMC (Intel Music Coder)"),
            support_audio_codec!(CODEC_ID_IAC, "iac", "IAC (Indeo Audio Coder)"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Audio;

    fn decoder(
        codec: symphonia_core::codecs::audio::AudioCodecId,
        rate: u32,
        ch: u32,
    ) -> ImcDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(codec).with_sample_rate(rate);
        p.with_channels(if ch == 2 {
            layouts::CHANNEL_LAYOUT_STEREO
        }
        else {
            layouts::CHANNEL_LAYOUT_MONO
        });
        ImcDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    /// Arbitrary (corrupt) input must never panic and must always yield finite output on success.
    #[test]
    fn verify_random_blocks_do_not_panic() {
        for (codec, ch) in [(CODEC_ID_IMC, 1), (CODEC_ID_IAC, 1), (CODEC_ID_IAC, 2)] {
            let mut dec = decoder(codec, 22050, ch);
            let mut state = 0x1234_5678_9abc_def0u64;
            let mut data = vec![0u8; IMC_BLOCK_SIZE * ch as usize * 2 + 5];
            for _ in 0..20000 {
                for b in data.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state >> 32) as u8;
                }
                // Keep the 9-bit header valid half of the time to reach deeper code paths.
                if state & 1 == 0 {
                    for blk in data.chunks_exact_mut(IMC_BLOCK_SIZE) {
                        blk[1] &= 0xe7;
                    }
                }
                let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
                if let Ok(GenericAudioBufferRef::F32(b)) = dec.decode_ref(&pkt) {
                    assert_eq!(b.frames(), 2 * COEFFS);
                }
            }
        }
    }

    #[test]
    fn verify_short_packet_is_error() {
        let mut dec = decoder(CODEC_ID_IMC, 22050, 1);
        let data = [0u8; IMC_BLOCK_SIZE - 1];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    #[test]
    fn verify_huffman_tables_are_complete() {
        // Every table must use the whole code space, as the reference decoder's tables do.
        for i in 0..4 {
            for j in 0..4 {
                let sum: u64 = HUFFMAN_LENS[i][j]
                    .iter()
                    .take(HUFFMAN_SIZES[i])
                    .map(|&l| 1u64 << (32 - u32::from(l)))
                    .sum();
                assert_eq!(sum, 1u64 << 32, "table {i}/{j}");
            }
        }
    }
}
