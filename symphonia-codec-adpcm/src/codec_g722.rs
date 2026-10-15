// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ITU-T G.722 sub-band ADPCM decoder (64, 56 and 48 kbit/s).
//!
//! The decoder is bit-exact with FFmpeg's `g722` decoder (`libavcodec/g722dec.c`, `g722.c` and
//! `g722dsp.c`), which in turn is a bit-exact implementation of the ITU-T G.722 reference
//! algorithm. All tables and integer-arithmetic details (including the 16-bit truncation of the
//! stored predictor/quantiser state) follow those sources:
//!
//! - <https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/g722dec.c>
//! - <https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/g722.c>
//! - <https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/g722dsp.c>
//!
//! Each input byte holds one codeword: the 2 most significant bits are the higher sub-band index
//! (`ihigh`), the next `6 - skip` bits are the lower sub-band index (`ilow`), and the remaining
//! `skip` least significant bits (`skip` = 0, 1 or 2 for 64, 56 and 48 kbit/s) are ignored. Each
//! codeword produces two 16 kHz output samples via the receive QMF synthesis filter.

use symphonia_core::errors::{Result, decode_error};

/// Number of past `rlow ± rhigh` values the QMF synthesis filter keeps. The filter has 24 taps and
/// each codeword contributes two new values, so 22 values of history are required.
const QMF_HISTORY: usize = 22;

/// Total number of QMF taps.
const QMF_TAPS: usize = QMF_HISTORY + 2;

/// Inverse log2 table used to derive the linear quantiser scale factor from the log scale factor
/// (`inv_log2_table`, FFmpeg `g722.c`).
const INV_LOG2_TABLE: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834,
    2896, 2960, 3025, 3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];

/// Higher sub-band log scale factor adaptation steps, indexed by `ihigh & 1`
/// (`high_log_factor_step`, FFmpeg `g722.c`).
const HIGH_LOG_FACTOR_STEP: [i32; 2] = [798, -214];

/// Higher sub-band 2-bit inverse quantiser output levels (`ff_g722_high_inv_quant`, FFmpeg
/// `g722.c`).
const HIGH_INV_QUANT: [i32; 4] = [-926, -202, 926, 202];

/// Lower sub-band log scale factor adaptation steps, indexed by the 4-bit lower sub-band index
/// (`low_log_factor_step`, FFmpeg `g722.c`).
const LOW_LOG_FACTOR_STEP: [i32; 16] =
    [-60, 3042, 1198, 538, 334, 172, 58, -30, 3042, 1198, 538, 334, 172, 58, -30, -60];

/// Lower sub-band 4-bit inverse quantiser output levels (`ff_g722_low_inv_quant4`, FFmpeg
/// `g722.c`). Used for the predictor adaptation in all bit-rate modes.
const LOW_INV_QUANT4: [i32; 16] =
    [0, -2557, -1612, -1121, -786, -530, -323, -150, 2557, 1612, 1121, 786, 530, 323, 150, 0];

/// Lower sub-band 5-bit inverse quantiser output levels used at 56 kbit/s (`low_inv_quant5`,
/// FFmpeg `g722dec.c`).
const LOW_INV_QUANT5: [i32; 32] = [
    -35, -35, -2919, -2195, -1765, -1458, -1219, -1023, -858, -714, -587, -473, -370, -276, -190,
    -110, 2919, 2195, 1765, 1458, 1219, 1023, 858, 714, 587, 473, 370, 276, 190, 110, 35, -35,
];

/// Lower sub-band 6-bit inverse quantiser output levels used at 64 kbit/s
/// (`ff_g722_low_inv_quant6`, FFmpeg `g722.c`).
const LOW_INV_QUANT6: [i32; 64] = [
    -17, -17, -17, -17, -3101, -2738, -2376, -2088, -1873, -1689, -1535, -1399, -1279, -1170,
    -1072, -982, -899, -822, -750, -682, -618, -558, -501, -447, -396, -347, -300, -254, -211,
    -170, -130, -91, 3101, 2738, 2376, 2088, 1873, 1689, 1535, 1399, 1279, 1170, 1072, 982, 899,
    822, 750, 682, 618, 558, 501, 447, 396, 347, 300, 254, 211, 170, 130, 91, 54, 17, -54, -17,
];

/// QMF synthesis coefficients for the odd history positions (these produce the first output
/// sample, `xout[0]`). The even history positions use the same coefficients in reverse order.
/// The twelve coefficients (3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11) are the
/// QMF coefficients of ITU-T G.722 Table 11, inlined in FFmpeg `g722_apply_qmf` (`g722dsp.c`).
const QMF_COEFFS_ODD: [i32; 12] = [-11, 53, -156, 362, -805, 3876, 951, -210, 32, 12, -11, 3];

/// QMF synthesis coefficients for the even history positions (these produce the second output
/// sample, `xout[1]`).
const QMF_COEFFS_EVEN: [i32; 12] = [3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11];

/// Adaptive predictor and quantiser state of one sub-band.
///
/// Field widths mirror FFmpeg's `struct G722Band`: values that FFmpeg stores in 16-bit fields are
/// kept as `i16` here so that any wrap-around behaviour is identical.
#[derive(Clone, Default)]
struct Band {
    /// Predictor output value.
    s_predictor: i16,
    /// Output of the zero (FIR) predictor section.
    s_zero: i32,
    /// Signs of the previous partially reconstructed signals (0 or 1).
    part_reconst_mem: [u8; 2],
    /// Previous quantised reconstructed signal.
    prev_qtzd_reconst: i16,
    /// Second-order pole section coefficients.
    pole_mem: [i16; 2],
    /// Quantised difference signal memory.
    diff_mem: [i32; 6],
    /// Sixth-order zero section coefficients.
    zero_mem: [i16; 6],
    /// Delayed base-2 logarithmic quantiser scale factor.
    log_factor: i16,
    /// Delayed linear quantiser scale factor.
    scale_factor: i16,
}

impl Band {
    fn new(scale_factor: i16) -> Self {
        Band { scale_factor, ..Default::default() }
    }

    /// Updates the zero predictor section with the current dequantised difference signal and
    /// computes its output.
    fn update_zero_section(&mut self, cur_diff: i32) {
        let mut s_zero = 0i32;

        // The taps are updated from the highest to the lowest index so `diff_mem[k - 1]` is still
        // the previous sample when it is shifted into `diff_mem[k]`.
        for k in (0..6).rev() {
            let x = if k == 0 { cur_diff * 2 } else { self.diff_mem[k - 1] };

            // With a zero difference the coefficients only leak.
            let step = if cur_diff == 0 {
                0
            }
            else if (self.diff_mem[k] ^ cur_diff) < 0 {
                -128
            }
            else {
                128
            };

            let zero = ((i32::from(self.zero_mem[k]) * 255) >> 8) + step;
            self.zero_mem[k] = zero as i16;
            self.diff_mem[k] = x;
            s_zero += (x * i32::from(self.zero_mem[k])) >> 15;
        }

        self.s_zero = s_zero;
    }

    /// Adaptive predictor update for the dequantised difference signal `cur_diff`.
    fn adapt_predictor(&mut self, cur_diff: i32) {
        let cur_part_reconst = u8::from(self.s_zero + cur_diff < 0);

        // Sign correlations used for the pole section adaptation.
        let sg0 = if cur_part_reconst != self.part_reconst_mem[0] { 1 } else { -1 };
        let sg1 = if cur_part_reconst == self.part_reconst_mem[1] { 1 } else { -1 };
        self.part_reconst_mem[1] = self.part_reconst_mem[0];
        self.part_reconst_mem[0] = cur_part_reconst;

        let pole0 = i32::from(self.pole_mem[0]);
        let pole1 = i32::from(self.pole_mem[1]);

        let new_pole1 =
            ((sg0 * pole0.clamp(-8191, 8191)) >> 5) + (sg1 * 128) + ((pole1 * 127) >> 7);
        let new_pole1 = new_pole1.clamp(-12288, 12288);
        self.pole_mem[1] = new_pole1 as i16;

        // `new_pole1` is within [-12288, 12288], so the limit is always positive.
        let limit = 15360 - new_pole1;
        let new_pole0 = (-192 * sg0 + ((pole0 * 255) >> 8)).clamp(-limit, limit);
        self.pole_mem[0] = new_pole0 as i16;

        self.update_zero_section(cur_diff);

        let cur_qtzd_reconst = clip_i16((i32::from(self.s_predictor) + cur_diff) * 2);
        self.s_predictor = clip_i16(
            self.s_zero
                + ((new_pole0 * cur_qtzd_reconst) >> 15)
                + ((new_pole1 * i32::from(self.prev_qtzd_reconst)) >> 15),
        ) as i16;
        self.prev_qtzd_reconst = cur_qtzd_reconst as i16;
    }

    /// Updates the lower sub-band predictor and quantiser with the 4-bit index `ilow4`.
    fn update_low(&mut self, ilow4: usize) {
        let cur_diff = (i32::from(self.scale_factor) * LOW_INV_QUANT4[ilow4]) >> 10;
        self.adapt_predictor(cur_diff);

        let log_factor = ((i32::from(self.log_factor) * 127) >> 7) + LOW_LOG_FACTOR_STEP[ilow4];
        let log_factor = log_factor.clamp(0, 18432);
        self.log_factor = log_factor as i16;
        self.scale_factor = linear_scale_factor(log_factor - (8 << 11)) as i16;
    }

    /// Updates the higher sub-band predictor and quantiser with the dequantised difference
    /// `dhigh` and the 2-bit index `ihigh`.
    fn update_high(&mut self, dhigh: i32, ihigh: usize) {
        self.adapt_predictor(dhigh);

        let log_factor =
            ((i32::from(self.log_factor) * 127) >> 7) + HIGH_LOG_FACTOR_STEP[ihigh & 1];
        let log_factor = log_factor.clamp(0, 22528);
        self.log_factor = log_factor as i16;
        self.scale_factor = linear_scale_factor(log_factor - (10 << 11)) as i16;
    }
}

/// Clips to the signed 16-bit range.
#[inline]
fn clip_i16(v: i32) -> i32 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))
}

/// Converts a (biased) log2 scale factor into the linear scale factor.
#[inline]
fn linear_scale_factor(log_factor: i32) -> i32 {
    let wd1 = INV_LOG2_TABLE[((log_factor >> 6) & 31) as usize];
    let shift = log_factor >> 11;
    if shift < 0 { wd1 >> shift.unsigned_abs() } else { wd1 << shift }
}

/// G.722 decoder.
#[derive(Clone)]
pub(crate) struct G722Decoder {
    /// Number of ignored least significant bits in each codeword: 0 (64 kbit/s), 1 (56 kbit/s) or
    /// 2 (48 kbit/s).
    skip: u32,
    /// Lower sub-band inverse quantiser for the active bit-rate mode.
    low_inv_quant: &'static [i32],
    /// Lower and higher sub-band states.
    low: Band,
    high: Band,
    /// QMF synthesis history; index 0 is the oldest value.
    qmf: [i16; QMF_TAPS],
}

impl G722Decoder {
    /// Creates a decoder for `bits_per_codeword` significant bits per 8-bit codeword: 8 (64
    /// kbit/s), 7 (56 kbit/s) or 6 (48 kbit/s). As in FFmpeg's `bits_per_codeword` option, the
    /// valid range is 6..=8; out-of-range values are clamped to it, callers are expected to reject
    /// them beforehand.
    pub(crate) fn new(bits_per_codeword: u32) -> Self {
        let skip = 8 - bits_per_codeword.clamp(6, 8);

        let low_inv_quant: &'static [i32] = match skip {
            0 => &LOW_INV_QUANT6,
            1 => &LOW_INV_QUANT5,
            _ => &LOW_INV_QUANT4,
        };

        G722Decoder {
            skip,
            low_inv_quant,
            low: Band::new(8),
            high: Band::new(2),
            qmf: [0; QMF_TAPS],
        }
    }

    /// Resets the decoder to its initial state.
    pub(crate) fn reset(&mut self) {
        self.low = Band::new(8);
        self.high = Band::new(2);
        self.qmf = [0; QMF_TAPS];
    }

    /// Decodes `data.len()` codewords into `out`, which must hold exactly two samples per input
    /// byte. Samples are signed 16-bit values shifted left by 16 into an `i32`.
    #[cfg(test)]
    pub(crate) fn decode(&mut self, data: &[u8], out: &mut [i32]) -> Result<()> {
        self.decode_channel(data, 0, 1, out)
    }

    /// Decodes the codewords of channel `ch` of `channels` (byte `i * channels + ch` of `data`)
    /// into `out`, which must hold exactly two samples per codeword.
    pub(crate) fn decode_channel(
        &mut self,
        data: &[u8],
        ch: usize,
        channels: usize,
        out: &mut [i32],
    ) -> Result<()> {
        if ch >= channels || out.len() != data.iter().skip(ch).step_by(channels).count() * 2 {
            return decode_error("adpcm (g722): output buffer must hold two samples per byte");
        }

        let skip = self.skip;
        // The 6 - skip lower sub-band bits sit directly under the 2 higher sub-band bits.
        let low_mask = (1usize << (6 - skip)) - 1;

        for (i, &byte) in data.iter().skip(ch).step_by(channels).enumerate() {
            let ihigh = usize::from(byte >> 6);
            let ilow = (usize::from(byte) >> skip) & low_mask;

            // Lower sub-band reconstruction.
            let dlow = (i32::from(self.low.scale_factor) * self.low_inv_quant[ilow]) >> 10;
            let rlow = (dlow + i32::from(self.low.s_predictor)).clamp(-(1 << 14), (1 << 14) - 1);
            self.low.update_low(ilow >> (2 - skip));

            // Higher sub-band reconstruction.
            let dhigh = (i32::from(self.high.scale_factor) * HIGH_INV_QUANT[ihigh]) >> 10;
            let rhigh = (dhigh + i32::from(self.high.s_predictor)).clamp(-(1 << 14), (1 << 14) - 1);
            self.high.update_high(dhigh, ihigh);

            // Both values fit in 16 bits: rlow and rhigh are in [-16384, 16383].
            self.qmf.copy_within(2.., 0);
            self.qmf[QMF_TAPS - 2] = (rlow + rhigh) as i16;
            self.qmf[QMF_TAPS - 1] = (rlow - rhigh) as i16;

            let (x0, x1) = apply_qmf(&self.qmf);
            out[2 * i] = clip_i16(x0 >> 11) << 16;
            out[2 * i + 1] = clip_i16(x1 >> 11) << 16;
        }

        Ok(())
    }
}

/// Applies the 24-tap QMF synthesis filter to the history, returning the two (unscaled) output
/// samples. The oldest history value is at index 0.
#[inline]
fn apply_qmf(history: &[i16; QMF_TAPS]) -> (i32, i32) {
    let mac = |first: usize, coeffs: &[i32; 12]| -> i32 {
        history.iter().skip(first).step_by(2).zip(coeffs).map(|(&h, &c)| i32::from(h) * c).sum()
    };

    (mac(1, &QMF_COEFFS_ODD), mac(0, &QMF_COEFFS_EVEN))
}

#[cfg(test)]
mod tests {
    use super::G722Decoder;

    /// 64 pseudo-random codewords (LCG, seed 1, `x = x * 1664525 + 1013904223`, byte = `x >> 24`).
    fn test_stream() -> Vec<u8> {
        let mut x = 1u32;
        (0..64)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 24) as u8
            })
            .collect()
    }

    /// FNV-1a (64-bit) of the output samples as little-endian s16.
    fn fnv1a(samples: &[i32]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for &s in samples {
            for byte in ((s >> 16) as i16).to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        hash
    }

    fn decode_all(bits: u32, data: &[u8]) -> Vec<i32> {
        let mut out = vec![0i32; data.len() * 2];
        G722Decoder::new(bits).decode(data, &mut out).expect("decode");
        out
    }

    /// Expected values were generated with FFmpeg n9.0.2:
    /// `ffmpeg -bits_per_codeword N -f g722 -i stream -f s16le -`.
    #[test]
    fn verify_against_ffmpeg() {
        let tail = |v: &[i32]| v[v.len() - 8..].iter().map(|s| s >> 16).collect::<Vec<_>>();

        let out = decode_all(8, &test_stream());
        assert_eq!(out.len(), 128);
        assert_eq!(tail(&out), [-9060, -21827, -11639, 11942, 16835, -1005, -15524, -13192]);
        assert_eq!(fnv1a(&out), 0x888a_dbc3_f012_b973);

        let out = decode_all(7, &test_stream());
        assert_eq!(tail(&out), [-8518, -20237, -10683, 10929, 15103, -1769, -15602, -13295]);
        assert_eq!(fnv1a(&out), 0x9b33_9b2f_4b71_6522);

        let out = decode_all(6, &test_stream());
        assert_eq!(tail(&out), [-9013, -23438, -12559, 13387, 18073, -2414, -17366, -12905]);
        assert_eq!(fnv1a(&out), 0xf74a_6453_b010_a3bf);
    }

    #[test]
    fn verify_state_persists_across_calls_and_reset() {
        let data = test_stream();
        let whole = decode_all(8, &data);

        let mut dec = G722Decoder::new(8);
        let mut chunked = vec![0i32; data.len() * 2];
        for (d, o) in data.chunks(5).zip(chunked.chunks_mut(10)) {
            dec.decode(d, o).expect("decode");
        }
        assert_eq!(whole, chunked);

        // After a reset the decoder behaves like a freshly created one.
        dec.reset();
        let mut again = vec![0i32; data.len() * 2];
        dec.decode(&data, &mut again).expect("decode");
        assert_eq!(whole, again);
    }

    #[test]
    fn verify_output_size_is_checked() {
        let mut dec = G722Decoder::new(8);
        assert!(dec.decode(&[0, 1], &mut [0i32; 3]).is_err());
        assert!(dec.decode(&[0, 1], &mut [0i32; 5]).is_err());
        assert!(dec.decode(&[], &mut []).is_ok());
    }
}
