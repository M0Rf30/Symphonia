// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The syntax of a frame and the reconstruction of its spectrum (`unpack`,
//! `dequantize_coefficients`, `reconstruct_hfr` and `apply_intensity_stereo` of FFmpeg's
//! `libavcodec/hcadec.c`).

use crate::header::BANDS;
use crate::tables::*;

/// The CRC-16 of a frame: polynomial 0x8005, most significant bit first, initial value 0
/// (`AV_CRC_16_ANSI`). It is 0 for a frame that ends with its own CRC.
const CRC_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// A reader of the bits of a frame, most significant bit first. Each byte is substituted through
/// `cipher` as it is read, so that an encrypted frame is decrypted without a copy. Reads past the
/// end of the frame give zero bits, as in FFmpeg; `overrun` tells if the position went past it.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    cipher: &'a [u8; 256],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8], cipher: &'a [u8; 256]) -> Self {
        BitReader { data, cipher, pos: 0 }
    }

    fn byte(&self, idx: usize) -> u32 {
        self.data.get(idx).map_or(0, |&b| u32::from(self.cipher[usize::from(b)]))
    }

    /// Reads `n <= 16` bits.
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }

        let idx = self.pos >> 3;
        let shift = (self.pos & 7) as u32;
        let window = (self.byte(idx) << 24)
            | (self.byte(idx + 1) << 16)
            | (self.byte(idx + 2) << 8)
            | self.byte(idx + 3);

        self.pos += n as usize;
        (window << shift) >> (32 - n)
    }

    /// Moves the position by `delta` bits.
    pub fn seek_by(&mut self, delta: isize) {
        self.pos = self.pos.saturating_add_signed(delta);
    }

    /// Returns true if more bits were read than the frame has.
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() * 8
    }

    /// The CRC-16 of the frame after the substitution. 0 if the frame is intact.
    pub fn crc(&self) -> u16 {
        self.data.iter().fold(0u16, |crc, &b| {
            let b = self.cipher[usize::from(b)];
            (crc << 8) ^ CRC_TABLE[usize::from((crc >> 8) as u8 ^ b)]
        })
    }
}

/// The state of a channel.
pub(crate) struct Channel {
    /// The dequantizer scale of each band.
    base: [f32; BANDS],
    /// The spectrum of the subframe.
    pub coefs: [f32; BANDS],
    /// The previous output of the transform, for the overlap.
    pub prev: [f32; BANDS],
    /// The scale factors; the entries after the coded bands are the scales of the high
    /// frequency groups.
    scale_factors: [i8; BANDS],
    scale: [u8; BANDS],
    intensity: [u8; 8],
    /// 0: mono, 1: the primary channel of a stereo pair, 2: the secondary channel of a pair.
    chan_type: u8,
    /// The number of bands that the channel codes.
    count: usize,
}

impl Channel {
    pub fn new(chan_type: u8, count: usize) -> Self {
        Channel {
            base: [0.0; BANDS],
            coefs: [0.0; BANDS],
            prev: [0.0; BANDS],
            scale_factors: [0; BANDS],
            scale: [0; BANDS],
            intensity: [0; 8],
            chan_type,
            count,
        }
    }

    /// Clears the state kept between frames.
    pub fn reset(&mut self) {
        self.prev = [0.0; BANDS];
    }

    /// Reads the scale factors and the high frequency or intensity parameters of a frame.
    /// The scales of the `hfr_group_count` high frequency groups are stored after the coded bands.
    pub fn unpack(
        &mut self,
        br: &mut BitReader<'_>,
        hfr_group_count: usize,
        packed_noise_level: i32,
        ath: &[u8; BANDS],
    ) {
        let count = self.count;
        let delta_bits = br.read(3);

        if delta_bits > 5 {
            for sf in &mut self.scale_factors[..count] {
                *sf = br.read(6) as i8;
            }
        }
        else if delta_bits > 0 {
            let mut factor = br.read(6) as i32;
            let max_value = (1i32 << delta_bits) - 1;
            let half_max = max_value >> 1;

            self.scale_factors[0] = factor as i8;

            for sf in &mut self.scale_factors[1..count.max(1)] {
                let delta = br.read(delta_bits) as i32;

                if delta == max_value {
                    factor = br.read(6) as i32;
                }
                else {
                    factor += delta - half_max;
                }

                factor = factor.clamp(0, 63);
                *sf = factor as i8;
            }
        }
        else {
            self.scale_factors = [0; BANDS];
        }

        if self.chan_type == 2 {
            self.intensity[0] = br.read(4) as u8;

            // Without the parameters of the other subframes, those of the previous frame stay, as
            // in FFmpeg.
            if self.intensity[0] < 15 {
                for v in &mut self.intensity[1..] {
                    *v = br.read(4) as u8;
                }
            }
        }
        else {
            for sf in &mut self.scale_factors[count..count + hfr_group_count] {
                *sf = br.read(6) as i8;
            }
        }

        for (i, &ath) in ath.iter().enumerate().take(count) {
            let mut scale = i32::from(self.scale_factors[i]);

            if scale != 0 {
                scale = i32::from(ath) + ((packed_noise_level + i as i32) >> 8)
                    - ((scale * 5) >> 1)
                    + 2;
                scale = i32::from(SCALE_TABLE[scale.clamp(0, 58) as usize]);
            }

            self.scale[i] = scale as u8;
        }

        self.scale[count..].fill(0);

        for i in 0..count {
            self.base[i] = DEQUANTIZER_SCALING[self.scale_factors[i] as usize]
                * QUANT_STEP_SIZE[usize::from(self.scale[i])];
        }
    }

    /// Reads the quantized spectrum of a subframe.
    pub fn dequantize(&mut self, br: &mut BitReader<'_>) {
        for i in 0..self.count {
            let scale = usize::from(self.scale[i]);
            let nb_bits = u32::from(MAX_BITS[scale]);
            let mut value = br.read(nb_bits) as i32;

            let factor = if scale > 7 {
                // The sign is the lowest bit. The code of 0 is one bit shorter.
                value = (1 - ((value & 1) << 1)) * (value >> 1);
                if value == 0 {
                    br.seek_by(-1);
                }
                value as f32
            }
            else {
                // The code is a prefix of the bits read.
                let idx = value as usize + (scale << 4);
                br.seek_by(isize::from(QUANT_SPECTRUM_BITS[idx]) - nb_bits as isize);
                f32::from(QUANT_SPECTRUM_VALUE[idx])
            };

            self.coefs[i] = factor * self.base[i];
        }

        self.coefs[self.count..].fill(0.0);
    }

    /// Fills the high frequency bands from the lower ones. The scales of the groups follow the
    /// `start_band` coded bands.
    pub fn reconstruct_hfr(
        &mut self,
        start_band: usize,
        hfr_group_count: usize,
        bands_per_hfr_group: usize,
        total_band_count: usize,
    ) {
        if self.chan_type == 2 || bands_per_hfr_group == 0 {
            return;
        }

        // The source band descends through all the groups.
        let mut k = start_band;
        let mut l = start_band;

        for i in 0..hfr_group_count {
            let hfr_scale = i32::from(self.scale_factors[start_band + i]);

            for _ in 0..bands_per_hfr_group {
                if k >= total_band_count || l == 0 {
                    break;
                }

                l -= 1;
                let delta = (hfr_scale - i32::from(self.scale_factors[l])).clamp(-64, 63);
                self.coefs[k] =
                    SCALE_CONVERSION[(SCALE_CONVERSION_BIAS + delta) as usize] * self.coefs[l];
                k += 1;
            }
        }

        self.coefs[BANDS - 1] = 0.0;
    }
}

/// Splits the primary channel `ch1` of a pair into the pair with the intensity ratio of the
/// subframe `index` of the secondary channel `ch2`.
pub(crate) fn apply_intensity_stereo(
    ch1: &mut Channel,
    ch2: &mut Channel,
    index: usize,
    band_count: usize,
    base_band_count: usize,
    stereo_band_count: usize,
) {
    if ch1.chan_type != 1 || stereo_band_count == 0 {
        return;
    }

    let ratio_l = INTENSITY_RATIO[usize::from(ch2.intensity[index])];
    let ratio_r = ratio_l - 2.0;

    let c1 = &mut ch1.coefs[base_band_count..base_band_count + band_count];
    let c2 = &mut ch2.coefs[base_band_count..base_band_count + band_count];

    for (a, b) in c1.iter_mut().zip(c2.iter_mut()) {
        *b = *a * ratio_r;
        *a *= ratio_l;
    }
}
