// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Siren / G.722.1 frame decoder.
//!
//! A frame is the envelope (the power of each 20-coefficient region, coded differentially), a rate
//! control value, and the vector quantized MLT coefficients of every region. The coefficient
//! decoding, noise filling, concealment and overlap-add follow FFmpeg's `libavcodec/siren.c`. The
//! bit allocation (`categorize`), including its adjustment for rates above one bit per sample, and
//! the 14 kHz (Annex C) parameters follow the ITU-T G.722.1 reference decoder (see `NOTICE`).

// The 0.70711 constants are the literal table values of the reference decoder, not sqrt(1/2);
// replacing them would change the output bits.
#![allow(clippy::approx_constant)]

use symphonia_core::errors::{Result, decode_error};

use crate::imdct::Imdct;
use crate::tables::{DECODER_TREES, DIFFERENTIAL_DECODER_TREE};

/// The number of coefficients in a region.
const REGION_SIZE: usize = 20;
/// The number of categories; the last one is pure noise substitution.
const NUM_CATEGORIES: usize = 8;
/// The maximum number of regions (Siren 14).
const MAX_REGIONS: usize = 28;
/// The maximum number of rate control possibilities (Siren 14).
const MAX_RATE_CONTROL_POSSIBILITIES: usize = 32;
/// The maximum frame size (Siren 14).
pub(crate) const MAX_FRAME_SIZE: usize = 640;

/// Bits per coefficient index in the packed vector index of each category.
const INDEX_TABLE: [u32; 7] = [4, 4, 3, 3, 2, 2, 1];
/// The number of coefficients in a vector of each category.
const VECTOR_DIMENSION: [usize; 7] = [2, 2, 2, 4, 4, 5, 5];
/// The number of vectors in a region of each category.
const NUMBER_OF_VECTORS: [usize; 7] = [10, 10, 10, 5, 5, 4, 4];
/// The expected number of bits used by a region of each category.
const EXPECTED_BITS_TABLE: [i32; NUM_CATEGORIES] = [52, 47, 43, 37, 29, 22, 16, 0];

/// Dequantization centroids by category, padded to the 16 values a packed index can address.
#[rustfmt::skip]
static MLT_QUANT: [[f32; 16]; 7] = [
    [0.0, 0.392, 0.761, 1.120, 1.477, 1.832, 2.183, 2.541, 2.893, 3.245, 3.598, 3.942, 4.288, 4.724, 0.0, 0.0],
    [0.0, 0.544, 1.060, 1.563, 2.068, 2.571, 3.072, 3.562, 4.070, 4.620, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 0.746, 1.464, 2.180, 2.882, 3.584, 4.316, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.006, 2.000, 2.993, 3.985, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.321, 2.703, 3.983, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.657, 3.491, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.964, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
];

/// Noise fill factors of category 5 by the number of non-zero coefficients in the region.
#[rustfmt::skip]
const NOISE_CATEGORY5: [f32; 21] = [
    0.70711, 0.6179, 0.5005, 0.3220, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678,
    0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.17678, 0.0,
];

/// Noise fill factors of category 6 by the number of non-zero coefficients in the region.
#[rustfmt::skip]
const NOISE_CATEGORY6: [f32; 21] = [
    0.70711, 0.5686, 0.3563, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25,
    0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.0,
];

/// Checksum masks of the MSN variant.
const CHECKSUM_TABLE: [u32; 4] = [0x7f80, 0x7878, 0x6666, 0x5555];

/// The parameters that distinguish the supported bitstream flavours.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    /// The number of samples (and coefficients) per frame.
    pub frame_size: usize,
    /// The number of coded 20-coefficient regions.
    pub regions: usize,
    /// The number of rate control bits.
    pub rate_control_bits: u32,
    /// The scale applied to dequantized coefficients.
    pub coef_scale: f32,
    /// The scale of the inverse transform.
    pub imdct_scale: f32,
    /// The offset between the coded and the actual power index of the first region.
    pub esf_adjustment: i32,
    /// Windows Live Messenger variant (FFmpeg `msnsiren`).
    pub microsoft: bool,
    /// Leading bits to skip.
    pub sample_rate_bits: usize,
    /// Trailing checksum bits, excluded from the coded data.
    pub checksum_bits: usize,
}

impl Config {
    /// Siren 7 (G.722.1): 16 kHz, 20 ms frames of 320 samples, 7 kHz audio bandwidth.
    pub(crate) fn siren7() -> Self {
        Config {
            frame_size: 320,
            regions: 14,
            rate_control_bits: 4,
            coef_scale: 22.0,
            imdct_scale: 1.0 / (22.0 * 32768.0),
            esf_adjustment: 7,
            microsoft: false,
            sample_rate_bits: 0,
            checksum_bits: 0,
        }
    }

    /// Siren 14 (G.722.1 Annex C): 32 kHz, 20 ms frames of 640 samples, 14 kHz audio bandwidth.
    pub(crate) fn siren14() -> Self {
        Config {
            frame_size: 640,
            regions: 28,
            rate_control_bits: 5,
            coef_scale: 33.0,
            imdct_scale: 1.0 / (33.0 * 32768.0) * SIREN14_TRANSFORM_GAIN,
            esf_adjustment: 7,
            microsoft: false,
            sample_rate_bits: 0,
            checksum_bits: 0,
        }
    }

    /// The Windows Live Messenger flavour of Siren 7.
    pub(crate) fn msn_siren() -> Self {
        Config {
            coef_scale: 1.0,
            esf_adjustment: -2,
            microsoft: true,
            sample_rate_bits: 2,
            checksum_bits: 4,
            ..Config::siren7()
        }
    }

    fn rate_control_possibilities(&self) -> usize {
        1 << self.rate_control_bits
    }

    fn valid_coefs(&self) -> usize {
        self.regions * REGION_SIZE
    }
}

/// The ratio between the Siren 14 and the Siren 7 inverse transform scales.
const SIREN14_TRANSFORM_GAIN: f32 = 1.0;

/// A most-significant-bit first reader over a frame. Reads past the end of the data return zeros,
/// like the zero padding FFmpeg's bit reader relies on; `left` reports the bits left in the frame.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    limit: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], limit: usize) -> Self {
        Bits { data, pos: 0, limit }
    }

    /// The number of bits remaining before the end of the frame (negative once overrun).
    fn left(&self) -> i64 {
        self.limit as i64 - self.pos as i64
    }

    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        u32::from(bit)
    }

    fn bits(&mut self, n: usize) -> u32 {
        (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
    }
}

/// The frame decoder and its inter-frame state.
pub(crate) struct FrameDecoder {
    cfg: Config,
    imdct: Imdct,
    /// Region standard deviation for every power index (`2^(index / 2)`).
    std_table: [f32; 64],
    window: Box<[f32]>,
    /// The (persistent) dequantized coefficients of the frame being decoded.
    coefs: Box<[f32]>,
    /// The coefficients of the last good frame, for concealment.
    backup: Box<[f32]>,
    /// The output of the transform of the current frame.
    cur: Box<[f32]>,
    /// The output of the transform of the previous frame.
    prev: Box<[f32]>,
    /// State of the noise generator.
    dw: [u32; 4],
    abs_power_index: [i32; MAX_REGIONS],
    region_std: [f32; MAX_REGIONS],
    power_categories: [i32; MAX_REGIONS],
    category_balance: [usize; MAX_RATE_CONTROL_POSSIBILITIES - 1],
}

impl FrameDecoder {
    pub(crate) fn new(cfg: Config) -> Self {
        let m = cfg.frame_size;

        let mut std_table = [0.0f32; 64];
        for (i, s) in std_table.iter_mut().enumerate() {
            *s = 2.0f64.powf((i as f64 - 24.0) / 2.0) as f32;
        }

        let window = (0..m)
            .map(|i| (((i as f64 + 0.5) * std::f64::consts::FRAC_PI_2) / m as f64).sin() as f32)
            .collect();

        FrameDecoder {
            cfg,
            imdct: Imdct::new(m, cfg.imdct_scale),
            std_table,
            window,
            coefs: vec![0.0; m].into_boxed_slice(),
            backup: vec![0.0; m].into_boxed_slice(),
            cur: vec![0.0; m].into_boxed_slice(),
            prev: vec![0.0; m].into_boxed_slice(),
            dw: [1; 4],
            abs_power_index: [0; MAX_REGIONS],
            region_std: [0.0; MAX_REGIONS],
            power_categories: [0; MAX_REGIONS],
            category_balance: [0; MAX_RATE_CONTROL_POSSIBILITIES - 1],
        }
    }

    /// Reset all state to that of a fresh decoder.
    pub(crate) fn reset(&mut self) {
        self.coefs.fill(0.0);
        self.backup.fill(0.0);
        self.cur.fill(0.0);
        self.prev.fill(0.0);
        self.dw = [1; 4];
    }

    /// Decode one frame of `limit` bits from the start of `data` into `pcm`, which must be
    /// `frame_size` samples long. On error the output is unspecified.
    pub(crate) fn decode(
        &mut self,
        data: &[u8],
        limit: usize,
        verify: bool,
        pcm: &mut [f32],
    ) -> Result<()> {
        let cfg = self.cfg;
        let mut r = Bits::new(data, limit);
        let checksum_bits = cfg.checksum_bits as i64;

        r.bits(cfg.sample_rate_bits);

        self.decode_envelope(&mut r)?;

        let rate_control = r.bits(cfg.rate_control_bits as usize) as usize;

        // Clamped so that arithmetic on it cannot overflow, whatever the size of the packet.
        let available = (r.left() - checksum_bits).clamp(-(1 << 24), 1 << 24) as i32;
        self.categorize(available)?;

        for &region in &self.category_balance[..rate_control] {
            self.power_categories[region] += 1;
        }

        let ran_out = self.decode_vector(&mut r);

        let possibilities = cfg.rate_control_possibilities();
        let mut frame_error = false;

        // Whatever remains of the frame must be padding of ones.
        let left = r.left() - checksum_bits;
        if !cfg.microsoft && ran_out {
            // The reference decoder flags a frame that ran out of bits as errored unless it uses the
            // highest rate control value.
            frame_error = rate_control + 1 < possibilities;
        }
        else if left > 0 {
            for _ in 0..left {
                frame_error |= r.bit() == 0;
            }
        }
        else if left < 0 && rate_control + 1 < possibilities {
            frame_error = true;
        }

        // Region power indices outside of the range an encoder can produce.
        let (min, max) = if cfg.microsoft {
            (-31, 33)
        }
        else {
            (-8 - cfg.esf_adjustment, 31 - cfg.esf_adjustment)
        };
        if self.abs_power_index[..cfg.regions].iter().any(|&p| p > max || p < min) {
            frame_error = true;
        }

        if verify && cfg.checksum_bits > 0 {
            let checksum = r.bits(cfg.checksum_bits);
            if msn_checksum(data, limit, checksum) != Some(checksum) {
                frame_error = true;
            }
        }

        // Conceal errors by repeating the last good frame.
        let valid = cfg.valid_coefs();
        if frame_error {
            self.coefs[..valid].copy_from_slice(&self.backup[..valid]);
            self.backup[..valid].fill(0.0);
        }
        else {
            self.backup[..valid].copy_from_slice(&self.coefs[..valid]);
        }

        for c in self.coefs.iter_mut().step_by(2) {
            *c = -*c;
        }

        self.imdct.imdct(&self.coefs, valid, &mut self.cur);

        // Overlap-add the previous and current transforms.
        let m = cfg.frame_size;
        let h = m / 2;
        for k in 0..h {
            let s0 = self.prev[h + k];
            let s1 = self.cur[h - 1 - k];
            let wi = self.window[k];
            let wj = self.window[m - 1 - k];
            pcm[k] = s0 * wj - s1 * wi;
            pcm[m - 1 - k] = s0 * wi + s1 * wj;
        }

        std::mem::swap(&mut self.cur, &mut self.prev);

        Ok(())
    }

    /// Decode the region power envelope.
    fn decode_envelope(&mut self, r: &mut Bits<'_>) -> Result<()> {
        let cfg = self.cfg;

        let first = (r.bits(5) as i32 - cfg.esf_adjustment).clamp(-24, 39);
        self.abs_power_index[0] = first;
        self.region_std[0] = self.std_table[(first + 24) as usize];

        for i in 1..cfg.regions {
            let mut index = 0i32;

            loop {
                if r.left() < (4 + cfg.regions - i + cfg.checksum_bits) as i64 {
                    return decode_error("siren: truncated envelope");
                }

                let bit = r.bit() as usize;
                index = i32::from(DIFFERENTIAL_DECODER_TREE[i - 1][index as usize][bit]);

                if index <= 0 {
                    break;
                }
            }

            let power = (self.abs_power_index[i - 1] - index - 12).clamp(-24, 39);
            self.abs_power_index[i] = power;
            self.region_std[i] = self.std_table[(power + 24) as usize];
        }

        Ok(())
    }

    /// Select a power category for each region such that the frame is expected to fit in
    /// `available` bits, and the order in which regions are to be moved to a higher category by
    /// the rate control value.
    fn categorize(&mut self, available: i32) -> Result<()> {
        let cfg = self.cfg;
        let regions = cfg.regions;
        let possibilities = cfg.rate_control_possibilities();
        let power = &self.abs_power_index[..regions];

        // At higher bit rates there is an increase for most categories in the average bit
        // consumption per region. The reference decoder compensates by pretending to have fewer
        // bits available than there are.
        let frame_size = cfg.frame_size as i32;
        let available = if available > frame_size {
            frame_size + (((available - frame_size) * 5) >> 3)
        }
        else {
            available
        };

        let category_of = |offset: i32, region: usize| -> i32 {
            ((offset - power[region]) >> 1).clamp(0, NUM_CATEGORIES as i32 - 1)
        };

        let mut offset = -32;
        let mut delta = 32;
        while delta > 0 {
            let test_offset = offset + delta;
            let expected: i32 = (0..regions)
                .map(|region| EXPECTED_BITS_TABLE[category_of(test_offset, region) as usize])
                .sum();
            if expected >= available - 32 {
                offset = test_offset;
            }
            delta /= 2;
        }

        let mut max_rate = [0i32; MAX_REGIONS];
        let mut min_rate = [0i32; MAX_REGIONS];
        let mut expected = 0;
        for region in 0..regions {
            let category = category_of(offset, region);
            max_rate[region] = category;
            min_rate[region] = category;
            expected += EXPECTED_BITS_TABLE[category as usize];
        }

        let (mut min, mut max) = (expected, expected);
        let mut balances = [0usize; 2 * MAX_RATE_CONTROL_POSSIBILITIES];
        let (mut min_ptr, mut max_ptr) = (possibilities, possibilities);

        for _ in 0..possibilities - 1 {
            if min + max > available * 2 {
                // Move the highest frequency region that benefits most to a lower rate category.
                let mut best = -99;
                let mut best_region = None;
                for region in (0..regions).rev() {
                    if min_rate[region] < 7 {
                        let t = offset - power[region] - 2 * min_rate[region];
                        if t > best {
                            best = t;
                            best_region = Some(region);
                        }
                    }
                }
                let Some(region) = best_region
                else {
                    return decode_error("siren: invalid bit allocation");
                };

                balances[min_ptr] = region;
                min_ptr += 1;
                let c = min_rate[region] as usize;
                min += EXPECTED_BITS_TABLE[c + 1] - EXPECTED_BITS_TABLE[c];
                min_rate[region] += 1;
            }
            else {
                // Move the lowest frequency region that benefits most to a higher rate category.
                let mut best = 99;
                let mut best_region = None;
                for region in 0..regions {
                    if max_rate[region] > 0 {
                        let t = offset - power[region] - 2 * max_rate[region];
                        if t < best {
                            best = t;
                            best_region = Some(region);
                        }
                    }
                }
                let Some(region) = best_region
                else {
                    return decode_error("siren: invalid bit allocation");
                };

                max_ptr -= 1;
                balances[max_ptr] = region;
                let c = max_rate[region] as usize;
                max += EXPECTED_BITS_TABLE[c - 1] - EXPECTED_BITS_TABLE[c];
                max_rate[region] -= 1;
            }
        }

        self.power_categories[..regions].copy_from_slice(&max_rate[..regions]);
        self.category_balance[..possibilities - 1]
            .copy_from_slice(&balances[max_ptr..max_ptr + possibilities - 1]);

        Ok(())
    }

    /// The next 16 bits of the noise generator (a lagged Fibonacci sequence).
    fn next_noise_word(&mut self) -> u32 {
        let [dw1, dw2, dw3, dw4] = self.dw;

        let mut word = dw1.wrapping_add(dw4);
        if word & 0x8000 != 0 {
            word = word.wrapping_add(1);
        }

        self.dw = [dw2, dw3, dw4, word];
        word
    }

    /// Decode the coefficients of every region. Returns true if the bits ran out, in which case
    /// the remaining regions have been noise filled.
    fn decode_vector(&mut self, r: &mut Bits<'_>) -> bool {
        let cfg = self.cfg;
        let checksum_bits = cfg.checksum_bits as i64;
        let scale = cfg.coef_scale;
        let mut error = false;

        for region in 0..cfg.regions {
            let mut category = self.power_categories[region];
            let std = self.region_std[region];
            let base = region * REGION_SIZE;

            if (0..7).contains(&category) {
                let cat = category as usize;
                let tree = DECODER_TREES[cat];
                let mask = (1usize << INDEX_TABLE[cat]) - 1;
                let mut pos = base;

                'vectors: for _ in 0..NUMBER_OF_VECTORS[cat] {
                    let mut index = 0usize;

                    loop {
                        if r.left() - checksum_bits <= 0 {
                            error = true;
                            break 'vectors;
                        }

                        match tree.get(index + r.bit() as usize) {
                            Some(&node) => index = usize::from(node),
                            None => {
                                error = true;
                                break 'vectors;
                            }
                        }

                        if index & 1 == 1 {
                            break;
                        }
                    }

                    index >>= 1;

                    for _ in 0..VECTOR_DIMENSION[cat] {
                        let mut value = MLT_QUANT[cat][index & mask];
                        index >>= INDEX_TABLE[cat];

                        if value != 0.0 {
                            if r.left() - checksum_bits <= 0 {
                                error = true;
                                break 'vectors;
                            }

                            value *= if r.bit() == 0 { -std } else { std };
                        }

                        self.coefs[pos] = value * scale;
                        pos += 1;
                    }
                }

                if error {
                    // Noise fill this and the remaining regions.
                    for c in &mut self.power_categories[region + 1..cfg.regions] {
                        *c = 7;
                    }
                    category = 7;
                }
            }

            let coefs = &mut self.coefs[base..base + REGION_SIZE];

            let noise = if category == 5 && cfg.microsoft {
                let mut n = 0;
                for &c in coefs.iter() {
                    if c != 0.0 {
                        n += 1;
                        if f64::from(c.abs()) > 2.0 * f64::from(std) {
                            n += 3;
                        }
                    }
                }

                if n >= NOISE_CATEGORY5.len() {
                    // FFmpeg abandons the remaining regions here.
                    error = true;
                    break;
                }

                std * NOISE_CATEGORY5[n]
            }
            else if category == 5 || category == 6 {
                let n = coefs.iter().filter(|&&c| c != 0.0).count();

                if category == 5 { std * NOISE_CATEGORY5[n] } else { std * NOISE_CATEGORY6[n] }
            }
            else if category == 7 {
                std * 0.70711
            }
            else {
                0.0
            };

            if (5..=7).contains(&category) {
                let mut dw1 = self.next_noise_word();
                let mut dw2 = self.next_noise_word();

                for pair in self.coefs[base..base + REGION_SIZE].chunks_exact_mut(2) {
                    if category == 7 || pair[0] == 0.0 {
                        pair[0] = if dw1 & 1 != 0 { noise } else { -noise };
                    }
                    dw1 >>= 1;

                    if category == 7 || pair[1] == 0.0 {
                        pair[1] = if dw2 & 1 != 0 { noise } else { -noise };
                    }
                    dw2 >>= 1;
                }
            }
        }

        error
    }
}

/// Compute the checksum of an MSN Siren frame. `checksum` is the value in the frame, whose bits
/// are excluded from the computation. FFmpeg's `siren_decode`.
fn msn_checksum(data: &[u8], limit: usize, checksum: u32) -> Option<u32> {
    let words = limit / 16;
    if words == 0 || data.len() < words * 2 {
        return None;
    }

    let word = |i: usize| (u32::from(data[2 * i]) << 8) | u32::from(data[2 * i + 1]);

    let mut sum = 0u32;
    for i in 0..words - 1 {
        sum ^= word(i) << (i % 15);
    }
    sum ^= (word(words - 1) & !checksum) << ((words - 1) % 15);
    sum = (sum >> 15) ^ (sum & 0x7fff);

    let mut calculated = 0;
    for mask in CHECKSUM_TABLE {
        let mut t = mask & sum;
        for shift in [8, 4, 2, 1] {
            t ^= t >> shift;
        }
        calculated = (calculated << 1) | (t & 1);
    }

    Some(calculated)
}
