// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC1 frame decoder. Port of FFmpeg's `libavcodec/atrac1.c` (LGPL-2.1+, see `NOTICE`).
//!
//! A frame holds one 212-byte sound unit per channel and decodes to 512 samples per channel. A
//! sound unit splits the spectrum in three QMF bands (low/mid/high, 128/128/256 samples), each of
//! which is coded with either one long MDCT or several short ones.

use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use crate::common::{
    QMF_DELAY, bits, iqmf_finish, iqmf_prepare, make_qmf_window, make_sf_table,
};
use crate::tables::{
    BFU_AMOUNT_TAB1, BFU_AMOUNT_TAB2, BFU_AMOUNT_TAB3, BFU_BANDS_T, BFU_START_LONG,
    BFU_START_SHORT, SPECS_PER_BFU,
};

/// Maximum number of block floating units in a sound unit.
const MAX_BFU: usize = 52;
/// Number of bytes in a sound unit.
pub(crate) const SU_SIZE: usize = 212;
/// Number of samples in a sound unit.
pub(crate) const SU_SAMPLES: usize = 512;
/// Number of bits in a sound unit.
const SU_MAX_BITS: usize = SU_SIZE * 8;
/// Maximum number of channels.
pub(crate) const MAX_CHANNELS: usize = 8;

const QMF_BANDS: usize = 3;
const IDX_HIGH_BAND: usize = 2;

/// Number of samples of each QMF band.
const SAMPLES_PER_BAND: [usize; QMF_BANDS] = [128, 128, 256];

/// Length of the overlap between two consecutive MDCT blocks.
const OVERLAP: usize = 16;
/// Delay in samples of the high band against the other bands.
const HIGH_BAND_DELAY: usize = 39;

/// Inverse MDCT scale: `1 / 2^15`. FFmpeg's `atrac1_decode_init` uses `-1 / 2^15` with its own
/// transform, whose sign convention is the opposite of Symphonia's.
const IMDCT_SCALE: f64 = 1.0 / 32768.0;

/// The state of one channel (`AT1SUCtx`).
struct SoundUnit {
    /// log2 of the number of MDCT blocks in each QMF band.
    log2_block_count: [usize; QMF_BANDS],
    /// IMDCT output of the current and the previous frame, for the overlap.
    spectrum: [[f32; SU_SAMPLES]; 2],
    /// Index of the current frame in `spectrum`.
    cur: usize,
    /// Delay line of the 1st stacked QMF filter.
    fst_qmf_delay: [f32; QMF_DELAY],
    /// Delay line of the 2nd stacked QMF filter.
    snd_qmf_delay: [f32; QMF_DELAY],
    /// Delay line of the high band.
    last_qmf_delay: [f32; 256 + HIGH_BAND_DELAY],
}

impl SoundUnit {
    fn new() -> Self {
        SoundUnit {
            log2_block_count: [0; QMF_BANDS],
            spectrum: [[0.0; SU_SAMPLES]; 2],
            cur: 0,
            fst_qmf_delay: [0.0; QMF_DELAY],
            snd_qmf_delay: [0.0; QMF_DELAY],
            last_qmf_delay: [0.0; 256 + HIGH_BAND_DELAY],
        }
    }
}

/// The ATRAC1 frame decoder (`AT1Ctx`).
pub(crate) struct Atrac1Core {
    units: Vec<SoundUnit>,
    dsp: Dsp,
}

/// Working buffers and tables shared by all channels.
struct Dsp {
    /// The MDCT spectrum of the current channel.
    spec: [f32; SU_SAMPLES],
    /// The time domain samples of the low, mid and high bands.
    bands: [[f32; 256]; QMF_BANDS],
    /// Inverse MDCTs of 32, 128 and 256 coefficients.
    imdct: [Imdct; 3],
    /// Output of the inverse MDCT (`2 * N` samples).
    imdct_out: [f32; 512],
    /// Output of the first QMF stage.
    qmf_tmp: [f32; 256],
    iqmf_temp: [f32; 512 + QMF_DELAY],
    sf_table: [f32; 64],
    qmf_window: [f32; 48],
    /// Overlap window: `sin((i + 0.5) * pi / 64)` for `i` in `0..32` (`ff_sine_32`).
    sine_window: [f32; 32],
}

/// Block size mode of a sound unit: log2 of the number of blocks of the low, mid and high bands.
fn parse_bsm(br: &mut BitReaderLtr<'_>) -> Result<[usize; QMF_BANDS]> {
    let mut log2_block_cnt = [0; QMF_BANDS];

    // Low and mid band.
    for cnt in log2_block_cnt.iter_mut().take(2) {
        let tmp = bits(br.read_bits_leq32(2))? as usize;
        if tmp & 1 != 0 {
            return decode_error("atrac1: invalid block size mode");
        }
        *cnt = 2 - tmp;
    }

    // High band.
    let tmp = bits(br.read_bits_leq32(2))? as usize;
    if tmp != 0 && tmp != 3 {
        return decode_error("atrac1: invalid block size mode");
    }
    log2_block_cnt[IDX_HIGH_BAND] = 3 - tmp;

    bits(br.ignore_bits(2))?;

    Ok(log2_block_cnt)
}

impl Atrac1Core {
    pub fn new(channels: usize) -> Result<Self> {
        if channels < 1 || channels > MAX_CHANNELS {
            return decode_error("atrac1: unsupported number of channels");
        }

        Ok(Atrac1Core {
            units: (0..channels).map(|_| SoundUnit::new()).collect(),
            dsp: Dsp::new(),
        })
    }

    pub fn reset(&mut self) {
        for u in &mut self.units {
            *u = SoundUnit::new();
        }
    }

    /// Number of bytes of a frame.
    pub fn frame_bytes(&self) -> usize {
        SU_SIZE * self.units.len()
    }

    /// Decodes one frame of `212 * channels` bytes into `out[ch][..512]`
    /// (`atrac1_decode_frame`).
    pub fn decode_frame(&mut self, data: &[u8], out: &mut [Vec<f32>]) -> Result<()> {
        for (ch, (su, out)) in self.units.iter_mut().zip(out.iter_mut()).enumerate() {
            let Some(su_data) = data.get(SU_SIZE * ch..SU_SIZE * (ch + 1))
            else {
                return decode_error("atrac1: not enough data to decode");
            };

            let mut br = BitReaderLtr::new(su_data);

            // Block size mode, 1st byte.
            su.log2_block_count = parse_bsm(&mut br)?;

            self.dsp.unpack_dequant(&mut br, &su.log2_block_count)?;
            self.dsp.imdct_block(su);
            self.dsp.subband_synthesis(su, out);
        }

        Ok(())
    }
}

impl Dsp {
    fn new() -> Self {
        let mut sine_window = [0.0f32; 32];
        for (i, w) in sine_window.iter_mut().enumerate() {
            *w = (((i as f64 + 0.5) / 64.0) * std::f64::consts::PI).sin() as f32;
        }

        Dsp {
            spec: [0.0; SU_SAMPLES],
            bands: [[0.0; 256]; QMF_BANDS],
            imdct: [
                Imdct::new_scaled(32, IMDCT_SCALE),
                Imdct::new_scaled(128, IMDCT_SCALE),
                Imdct::new_scaled(256, IMDCT_SCALE),
            ],
            imdct_out: [0.0; 512],
            qmf_tmp: [0.0; 256],
            iqmf_temp: [0.0; 512 + QMF_DELAY],
            sf_table: make_sf_table(),
            qmf_window: make_qmf_window(),
            sine_window,
        }
    }

    /// Reads the coded spectrum of a sound unit and dequantizes it (`at1_unpack_dequant`).
    fn unpack_dequant(
        &mut self,
        br: &mut BitReaderLtr<'_>,
        log2_block_count: &[usize; QMF_BANDS],
    ) -> Result<()> {
        let sf_table = &self.sf_table;
        let spec = &mut self.spec;

        let mut idwls = [0u8; MAX_BFU];
        let mut idsfs = [0u8; MAX_BFU];

        // The info byte tells how many BFUs were coded.
        let num_bfus = usize::from(BFU_AMOUNT_TAB1[bits(br.read_bits_leq32(3))? as usize]);

        // Bits consumed: num_bfus * (idwl (4 bits) + idsf (6 bits)) + log2_block_count (8 bits) +
        // info byte (8 bits) + info byte copy (8 bits) + log2_block_count copy (8 bits).
        let mut bits_used = num_bfus * 10
            + 32
            + usize::from(BFU_AMOUNT_TAB2[bits(br.read_bits_leq32(2))? as usize])
            + (usize::from(BFU_AMOUNT_TAB3[bits(br.read_bits_leq32(3))? as usize]) << 1);

        // Word length index of each BFU.
        for idwl in idwls.iter_mut().take(num_bfus) {
            *idwl = bits(br.read_bits_leq32(4))? as u8;
        }

        // Scale factor index of each BFU.
        for idsf in idsfs.iter_mut().take(num_bfus) {
            *idsf = bits(br.read_bits_leq32(6))? as u8;
        }

        // Read the spectral data and reconstruct the MDCT spectrum of this channel.
        for band in 0..QMF_BANDS {
            let first = usize::from(BFU_BANDS_T[band]);
            let last = usize::from(BFU_BANDS_T[band + 1]);

            for bfu in first..last {
                let num_specs = usize::from(SPECS_PER_BFU[bfu]);
                let idwl = u32::from(idwls[bfu]);
                let word_len = if idwl != 0 { idwl + 1 } else { 0 };
                let scale_factor = sf_table[usize::from(idsfs[bfu])];

                // Add the number of bits consumed by this BFU and check for overflow.
                bits_used += word_len as usize * num_specs;
                if bits_used > SU_MAX_BITS {
                    return decode_error("atrac1: bitstream overflow");
                }

                // Position of the first spec according to the block size mode.
                let pos = if log2_block_count[band] != 0 {
                    usize::from(BFU_START_SHORT[bfu])
                }
                else {
                    usize::from(BFU_START_LONG[bfu])
                };

                let dst = &mut spec[pos..pos + num_specs];

                if word_len != 0 {
                    let max_quant = (1.0f64 / f64::from((1u32 << (word_len - 1)) - 1)) as f32;

                    for s in dst.iter_mut() {
                        // Read a quantized spec and dequantize it.
                        let q = bits(br.read_bits_leq32_signed(word_len))?;
                        *s = q as f32 * scale_factor * max_quant;
                    }
                }
                else {
                    // Empty BFU.
                    dst.fill(0.0);
                }
            }
        }

        Ok(())
    }

    /// Inverse MDCT of the `out.len()` coefficients of `self.spec` at `pos`, optionally reversed
    /// (the odd bands are reversed by the QMF), into `out`. The output is the half of the full
    /// IMDCT output that FFmpeg's `av_tx` keeps.
    fn imdct(&mut self, pos: usize, rev_spec: bool, out: &mut [f32]) {
        let n = out.len();
        let spec = &mut self.spec[pos..pos + n];

        if rev_spec {
            spec.reverse();
        }

        let idx = match n {
            32 => 0,
            128 => 1,
            _ => 2,
        };

        let full = &mut self.imdct_out[..2 * n];
        self.imdct[idx].imdct(spec, full);

        out.copy_from_slice(&full[n / 2..n / 2 + n]);
    }

    /// IMDCT, windowing and overlap of the three bands (`at1_imdct_block`).
    fn imdct_block(&mut self, su: &mut SoundUnit) {
        let mut ref_pos = 0;
        let mut pos = 0;

        let cur = su.cur;
        let prev = 1 - cur;

        for (band, &band_samples) in SAMPLES_PER_BAND.iter().enumerate().take(QMF_BANDS) {

            // Number of MDCT blocks in the band: 1 for long mode, 4 for short mode (low/mid
            // bands) and 8 for short mode (high band).
            let num_blocks = 1usize << su.log2_block_count[band];

            // MDCT block size in samples: 128 (long mode, low & mid bands), 256 (long mode, high
            // band) and 32 (short mode, all bands).
            let block_size = band_samples / num_blocks;

            let mut start_pos = 0;

            for j in 0..num_blocks {
                let out_pos = ref_pos + start_pos;

                self.imdct(pos, band != 0, &mut su.spectrum[cur][out_pos..out_pos + block_size]);

                // The tail of the previous block, to overlap with.
                let prev_tail: &[f32] = if j == 0 {
                    &su.spectrum[prev][ref_pos + band_samples - OVERLAP..ref_pos + band_samples]
                }
                else {
                    let p = out_pos - block_size + OVERLAP;
                    &su.spectrum[cur][p..p + OVERLAP]
                };

                // Overlap and window.
                let head = &su.spectrum[cur][out_pos..out_pos + OVERLAP];
                fmul_window(
                    &mut self.bands[band][start_pos..start_pos + 2 * OVERLAP],
                    prev_tail,
                    head,
                    &self.sine_window,
                );

                start_pos += block_size;
                pos += block_size;
            }

            if num_blocks == 1 {
                let src = &su.spectrum[cur][ref_pos + OVERLAP..ref_pos + band_samples - OVERLAP];
                self.bands[band][2 * OVERLAP..band_samples].copy_from_slice(src);
            }

            ref_pos += band_samples;
        }

        // Swap the buffers so that the MDCT overlap works.
        su.cur = prev;
    }

    /// Combines the three QMF bands into 512 samples (`at1_subband_synthesis`).
    fn subband_synthesis(&mut self, su: &mut SoundUnit, out: &mut [f32]) {
        // Combine the low and the middle bands.
        iqmf_prepare(
            &self.bands[0][..128],
            &self.bands[1][..128],
            &su.fst_qmf_delay,
            &mut self.iqmf_temp,
        );
        iqmf_finish(&self.qmf_window, 128, &self.iqmf_temp, &mut self.qmf_tmp, &mut su.fst_qmf_delay);

        // Delay the signal of the high band by 39 samples.
        su.last_qmf_delay.copy_within(256..256 + HIGH_BAND_DELAY, 0);
        su.last_qmf_delay[HIGH_BAND_DELAY..].copy_from_slice(&self.bands[2]);

        // Combine (low + middle) and high bands.
        iqmf_prepare(
            &self.qmf_tmp,
            &su.last_qmf_delay[..256],
            &su.snd_qmf_delay,
            &mut self.iqmf_temp,
        );
        iqmf_finish(&self.qmf_window, 256, &self.iqmf_temp, out, &mut su.snd_qmf_delay);
    }
}

/// Windowed overlap of two sequences (`vector_fmul_window` with `len` = `src0.len()`).
///
/// `dst` receives `2 * len` samples from the `len` samples of `src0` and the `len` samples of
/// `src1`, which is read in reverse.
fn fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32]) {
    let len = src0.len();

    for k in 0..len {
        let j = len - 1 - k;

        let s0 = src0[k];
        let s1 = src1[j];
        let wi = win[k];
        let wj = win[2 * len - 1 - k];

        dst[k] = s0 * wj - s1 * wi;
        dst[2 * len - 1 - k] = s0 * wi + s1 * wj;
    }
}
