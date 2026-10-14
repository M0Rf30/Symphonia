// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! E-AC-3 extensions: the adaptive hybrid transform, spectral extension, and enhanced coupling.

use crate::bits::BitReader;
use crate::core::{CPL, Core, Invalid, Lfg, Res};
use crate::tables::{
    BITS_VS_HEBAP, GAQ_REMAP_1, GAQ_REMAP_2_A, GAQ_REMAP_2_B, GAQ_REMAP_4_A, GAQ_REMAP_4_B,
    SPX_ATTEN, VQ_HEBAP1, VQ_HEBAP2, VQ_HEBAP3, VQ_HEBAP4, VQ_HEBAP5, VQ_HEBAP6, VQ_HEBAP7,
};

macro_rules! bail {
    ($cond:expr) => {
        if $cond {
            return Err(Invalid);
        }
    };
}

/// Enhanced coupling sub-band start bins (indexed by sub-band, ETSI TS 102 366 Table E.2.9).
const ECPL_SUBBAND_START: [usize; 23] = [
    13, 19, 25, 31, 37, 49, 61, 73, 85, 97, 109, 121, 133, 145, 157, 169, 181, 193, 205, 217, 229,
    241, 253,
];

const DEFAULT_SPX_BAND_STRUCT: [u8; 17] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1];

const DEFAULT_ECPL_BAND_STRUCT: [u8; 22] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 0, 1, 1, 1];

/// 24-bit fixed-point constants for the 6-point IDCT of the adaptive hybrid transform.
const COEFF_0: i64 = 10_273_905;
const COEFF_1: i64 = 11_863_283;
const COEFF_2: i64 = 3_070_444;

/// The 6-point inverse DCT of the AHT pre-mantissas, in 24-bit fixed-point.
fn idct6(p: &mut [i32; 6]) {
    let odd1 = p[1].wrapping_sub(p[3]).wrapping_sub(p[5]);

    let even2 = ((i64::from(p[2]) * COEFF_0) >> 23) as i32;
    let tmp = ((i64::from(p[4]) * COEFF_1) >> 23) as i32;
    let odd0 = ((i64::from(p[1].wrapping_add(p[5])) * COEFF_2) >> 23) as i32;

    let even0 = p[0].wrapping_add(tmp >> 1);
    let even1 = p[0].wrapping_sub(tmp);

    let tmp = even0;
    let even0 = tmp.wrapping_add(even2);
    let even2 = tmp.wrapping_sub(even2);

    let tmp = odd0;
    let odd0 = tmp.wrapping_add(p[1]).wrapping_add(p[3]);
    let odd2 = tmp.wrapping_add(p[5]).wrapping_sub(p[3]);

    p[0] = even0.wrapping_add(odd0);
    p[1] = even1.wrapping_add(odd1);
    p[2] = even2.wrapping_add(odd2);
    p[3] = even2.wrapping_sub(odd2);
    p[4] = even1.wrapping_sub(odd1);
    p[5] = even0.wrapping_sub(odd0);
}

/// Returns the vector quantization codebook entry (6 transform blocks) for `hebap` 1 to 7.
#[inline]
fn vq_entry(hebap: u8, index: usize) -> &'static [i16; 6] {
    // The index is at most `2^bits - 1`, where `bits` is the codebook index width, so it is always
    // within the codebook; the fallbacks only exist to keep the lookups panic-free.
    const ZERO: [i16; 6] = [0; 6];
    match hebap {
        1 => VQ_HEBAP1.get(index),
        2 => VQ_HEBAP2.get(index),
        3 => VQ_HEBAP3.get(index),
        4 => VQ_HEBAP4.get(index),
        5 => VQ_HEBAP5.get(index),
        6 => VQ_HEBAP6.get(index),
        _ => VQ_HEBAP7.get(index),
    }
    .unwrap_or(&ZERO)
}

impl Core {
    /// Decodes the adaptive hybrid transform mantissas of all six blocks of channel `ch`.
    pub(crate) fn decode_aht_ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        lfg: &mut Lfg,
    ) -> Res {
        let (start, end) = (self.start_freq[ch], self.end_freq[ch].min(256));

        let gaq_mode = br.get(2);
        let end_bap = if gaq_mode < 2 { 12 } else { 17 };

        // Gain codes for bins with a hebap between 8 and end_bap.
        let mut gaq_gain = [0u8; 256 + 2];
        let mut gs = 0;

        if gaq_mode == 1 || gaq_mode == 2 {
            for bin in start..end {
                let hebap = self.bap[ch][bin];
                if hebap > 7 && hebap < end_bap {
                    gaq_gain[gs] = (br.get(1) << (gaq_mode - 1)) as u8;
                    gs += 1;
                }
            }
        }
        else if gaq_mode == 3 {
            // Three gain codes are packed in 5 bits.
            let mut gc = 2;
            for bin in start..end {
                let hebap = self.bap[ch][bin];
                if hebap > 7 && hebap < 17 {
                    if gc == 2 {
                        let code = br.get(5).min(26) as u8;
                        gaq_gain[gs] = code / 9;
                        gaq_gain[gs + 1] = (code % 9) / 3;
                        gaq_gain[gs + 2] = code % 3;
                        gs += 3;
                        gc = 0;
                    }
                    else {
                        gc += 1;
                    }
                }
            }
        }

        let mut gs = 0;
        for bin in start..end {
            let hebap = self.bap[ch][bin];
            let bits = u32::from(BITS_VS_HEBAP[usize::from(hebap)]);

            if hebap == 0 {
                // Zero-mantissa dithering.
                for blk in 0..6 {
                    self.pre_mant[ch][bin][blk] = (lfg.get() & 0x7f_ffff) as i32 - 0x40_0000;
                }
            }
            else if hebap < 8 {
                // Vector quantization.
                let v = br.get(bits) as usize;
                let entry = vq_entry(hebap, v);
                for blk in 0..6 {
                    self.pre_mant[ch][bin][blk] = i32::from(entry[blk]) << 8;
                }
            }
            else {
                // Gain adaptive quantization.
                let log_gain = if gaq_mode != 0 && hebap < end_bap {
                    let g = gaq_gain[gs];
                    gs += 1;
                    u32::from(g)
                }
                else {
                    0
                };

                bail!(bits <= log_gain);
                let gbits = bits - log_gain;
                let idx = usize::from(hebap - 8);

                for blk in 0..6 {
                    let mut mant = br.get_signed(gbits);
                    if log_gain != 0 && mant == -(1 << (gbits - 1)) {
                        // Large mantissa.
                        let mbits = bits - (2 - log_gain);
                        mant = br.get_signed(mbits).wrapping_shl(23 - (mbits - 1));
                        // Correct for the asymmetric quantization.
                        let (a, b_tab) = if log_gain == 1 {
                            (GAQ_REMAP_2_A[idx], GAQ_REMAP_2_B[idx])
                        }
                        else {
                            (GAQ_REMAP_4_A[idx], GAQ_REMAP_4_B[idx])
                        };
                        let b =
                            if mant >= 0 { 1i32 << (23 - log_gain) } else { i32::from(b_tab) << 8 };
                        mant = mant
                            .wrapping_add(((i64::from(a) * i64::from(mant)) >> 15) as i32)
                            .wrapping_add(b);
                    }
                    else {
                        // Small mantissa, no GAQ, or Gk = 1.
                        mant = mant.wrapping_mul(1 << (24 - bits));
                        if log_gain == 0 {
                            mant = mant.wrapping_add(
                                ((i64::from(GAQ_REMAP_1[idx]) * i64::from(mant)) >> 15) as i32,
                            );
                        }
                    }
                    self.pre_mant[ch][bin][blk] = mant;
                }
            }

            idct6(&mut self.pre_mant[ch][bin]);
        }

        bail!(br.overrun());
        Ok(())
    }

    /// Parses the spectral extension strategy.
    pub(crate) fn spx_strategy(&mut self, br: &mut BitReader<'_>) -> Res {
        // The channels using spectral extension.
        if self.acmod == 1 {
            self.channel_uses_spx[1] = true;
        }
        else {
            for ch in 1..=self.fbw {
                self.channel_uses_spx[ch] = br.bit();
            }
        }

        let dst_start = br.get(2) as usize * 12 + 25;

        let mut start_sb = br.get(3) as usize + 2;
        if start_sb > 7 {
            start_sb += start_sb - 7;
        }
        let mut end_sb = br.get(3) as usize + 5;
        if end_sb > 7 {
            end_sb += end_sb - 7;
        }

        let src_start = start_sb * 12 + 25;
        let dst_end = end_sb * 12 + 25;

        bail!(start_sb >= end_sb);
        bail!(dst_start >= src_start);

        self.spx_dst_start_freq = dst_start;
        self.spx_src_start_freq = src_start;
        self.spx_dst_end_freq = dst_end;
        self.spx_start_subband = start_sb;

        self.num_spx_bands = Self::decode_band_structure(
            br,
            true,
            start_sb,
            end_sb,
            &DEFAULT_SPX_BAND_STRUCT,
            !self.spx_struct_valid,
            &mut self.spx_band_struct,
            &mut self.spx_band_sizes,
        );
        self.spx_struct_valid = true;

        Ok(())
    }

    /// Parses the spectral extension coordinates.
    pub(crate) fn spx_coordinates(&mut self, br: &mut BitReader<'_>) {
        for ch in 1..=self.fbw {
            if !self.channel_uses_spx[ch] {
                self.first_spx_coords[ch] = true;
                continue;
            }

            if self.first_spx_coords[ch] || br.bit() {
                self.first_spx_coords[ch] = false;

                let spx_blend = br.get(5) as f32 * (1.0 / 32.0);
                let master = 3 * br.get(2);

                let mut bin = self.spx_src_start_freq;
                for bnd in 0..self.num_spx_bands {
                    let bandsize = usize::from(self.spx_band_sizes[bnd]);

                    // Blending factors.
                    let nratio = (((bin + (bandsize >> 1)) as f32) / self.spx_dst_end_freq as f32
                        - spx_blend)
                        .clamp(0.0, 1.0);
                    // The noise is scaled by sqrt(3) to give unity variance.
                    let nblend = (3.0 * nratio).sqrt();
                    let sblend = (1.0 - nratio).sqrt();
                    bin += bandsize;

                    let exp = br.get(4);
                    let mut mant = br.get(2);
                    if exp == 15 {
                        mant <<= 1;
                    }
                    else {
                        mant += 4;
                    }
                    // Shift in 64 bits: corrupt streams may request more than 32 bits.
                    let shift = 25i32 - exp as i32 - master as i32;
                    let mant = if shift >= 0 {
                        (u64::from(mant) << shift.min(40)) as f32
                    }
                    else {
                        (u64::from(mant) >> (-shift).min(63)) as f32
                    };

                    let coord = mant * (1.0 / (1 << 23) as f32);
                    self.spx_noise_blend[ch][bnd] = nblend * coord;
                    self.spx_signal_blend[ch][bnd] = sblend * coord;
                }
            }
        }
    }

    /// Reconstructs the high frequency bins of the channels that use spectral extension.
    pub(crate) fn apply_spectral_extension(&mut self, lfg: &mut Lfg) {
        let dst_start = self.spx_dst_start_freq;
        let src_start = self.spx_src_start_freq;
        let nbands = self.num_spx_bands;

        // Build the copy index mapping and the wrap flags for the notch filter.
        let mut wrapflag = [false; 17];
        wrapflag[0] = true;
        let mut copy_sizes = [0usize; 40];
        let mut num_copy = 0;

        let mut bin = dst_start;
        for bnd in 0..nbands {
            let bandsize = usize::from(self.spx_band_sizes[bnd]);
            if bin + bandsize > src_start {
                copy_sizes[num_copy] = bin - dst_start;
                num_copy += 1;
                bin = dst_start;
                wrapflag[bnd] = true;
            }
            let mut i = 0;
            while i < bandsize {
                if bin == src_start {
                    copy_sizes[num_copy] = bin - dst_start;
                    num_copy += 1;
                    bin = dst_start;
                }
                let copysize = (bandsize - i).min(src_start - bin);
                bin += copysize;
                i += copysize;
            }
        }
        copy_sizes[num_copy] = bin - dst_start;
        num_copy += 1;

        for ch in 1..=self.fbw {
            if !self.channel_uses_spx[ch] {
                continue;
            }
            let tc = &mut self.tc[ch];

            // Copy coefficients from the normal bands to the extension bands.
            let mut bin = src_start;
            for &size in &copy_sizes[..num_copy] {
                tc.copy_within(dst_start..dst_start + size, bin);
                bin += size;
            }

            // RMS energy of each band.
            let mut rms = [0.0f32; 17];
            let mut bin = src_start;
            for bnd in 0..nbands {
                let bandsize = usize::from(self.spx_band_sizes[bnd]);
                let mut accum = 0.0f32;
                for &c in &tc[bin..bin + bandsize] {
                    accum += c * c;
                }
                bin += bandsize;
                rms[bnd] = (accum / bandsize as f32).sqrt();
            }

            // Notch filter at the transitions between normal and extension bands and at all wrap
            // points.
            if self.spx_atten_code[ch] >= 0 {
                let atten = &SPX_ATTEN[self.spx_atten_code[ch] as usize & 31];
                let mut bin = src_start - 2;
                for bnd in 0..nbands {
                    if wrapflag[bnd] {
                        tc[bin] *= atten[0];
                        tc[bin + 1] *= atten[1];
                        tc[bin + 2] *= atten[2];
                        tc[bin + 3] *= atten[1];
                        tc[bin + 4] *= atten[0];
                    }
                    bin += usize::from(self.spx_band_sizes[bnd]);
                }
            }

            // Noise blended scaling.
            let mut bin = src_start;
            for bnd in 0..nbands {
                let nscale = self.spx_noise_blend[ch][bnd] * rms[bnd] * (1.0 / i32::MIN as f32);
                let sscale = self.spx_signal_blend[ch][bnd];
                for _ in 0..self.spx_band_sizes[bnd] {
                    let noise = nscale * (lfg.get() as i32) as f32;
                    tc[bin] = tc[bin] * sscale + noise;
                    bin += 1;
                }
            }
        }
    }

    /// Parses the enhanced coupling strategy.
    pub(crate) fn ecpl_strategy(&mut self, br: &mut BitReader<'_>) -> Res {
        let begf = br.get(4) as usize;
        let begin = match begf {
            0..=2 => begf * 2,
            3..=12 => begf + 2,
            _ => begf * 2 - 10,
        };
        let end = if self.spx_in_use { self.spx_start_subband + 3 } else { br.get(4) as usize + 7 };
        bail!(begin >= end || end > 22);

        self.ecpl_begf = begf;
        self.ecpl_begin_subbnd = begin;
        self.ecpl_end_subbnd = end;
        self.start_freq[CPL] = ECPL_SUBBAND_START[begin];
        self.end_freq[CPL] = ECPL_SUBBAND_START[end];

        if !self.ecpl_struct_valid {
            self.ecpl_band_struct = DEFAULT_ECPL_BAND_STRUCT;
        }
        if br.bit() {
            for sbnd in (begin + 1).max(9)..end {
                self.ecpl_band_struct[sbnd] = br.get(1) as u8;
            }
        }

        // Band edges (in sub-bands).
        let mut n = 0;
        for sbnd in begin..end {
            if sbnd == begin || self.ecpl_band_struct[sbnd] == 0 {
                self.ecpl_band_edges[n] = sbnd as u8;
                n += 1;
            }
        }
        self.ecpl_band_edges[n] = end as u8;
        self.num_ecpl_bands = n;

        Ok(())
    }

    /// Parses the enhanced coupling amplitudes.
    pub(crate) fn ecpl_coordinates(&mut self, br: &mut BitReader<'_>) -> Res {
        let nbands = self.num_ecpl_bands;
        let mut first_in_cpl = None;

        // Reserved bit.
        br.skip(1);

        for ch in 1..=self.fbw {
            if self.channel_in_cpl[ch] {
                let first = *first_in_cpl.get_or_insert(ch);

                let (param1e, rsvd) = if self.first_cpl_coords[ch] {
                    self.first_cpl_coords[ch] = false;
                    (true, ch > first)
                }
                else {
                    let p = br.bit();
                    (p, ch > first && br.bit())
                };

                if param1e {
                    for bnd in 0..nbands {
                        self.ecpl_amp[ch][bnd] = br.get(5) as u8;
                    }
                }
                if rsvd {
                    br.skip(9 * (nbands - 1));
                }
                if ch > first {
                    br.skip(1);
                }
            }
            else {
                self.first_cpl_coords[ch] = true;
            }
        }

        Ok(())
    }

    /// Generates the coefficients of the coupled channels from the enhanced coupling channel.
    pub(crate) fn calc_transform_coeffs_ecpl(&mut self) {
        for band in 0..self.num_ecpl_bands {
            let lo = ECPL_SUBBAND_START[usize::from(self.ecpl_band_edges[band])];
            let hi = ECPL_SUBBAND_START[usize::from(self.ecpl_band_edges[band + 1])].min(256);

            for ch in 1..=self.fbw {
                if !self.channel_in_cpl[ch] {
                    continue;
                }

                // ampbnd = (ecplampmanttab / 32) >> ecplampexptab
                let code = u32::from(self.ecpl_amp[ch][band]);
                let (mant, shift) = match code {
                    0 => (0x20, 5),
                    1..=30 => {
                        ([0x1b, 0x17, 0x13, 0x10][((code - 1) % 4) as usize], 5 + (code - 1) / 4)
                    }
                    _ => (0, 5),
                };

                for b in lo..hi {
                    self.fixed[ch][b] = ((i64::from(self.fixed[CPL][b]) * mant) >> shift) as i32;
                }
            }
        }
    }
}
