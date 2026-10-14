// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dca_core.c, dca.c, dcamath.h, dcaadpcm.h (LGPL-2.1+), see NOTICE.
// The fixed-point synthesis path additionally follows dcadct.c, dcadsp.c, synth_filter.c and
// libavutil/fixed_dsp.c (LGPL-2.1+).

//! DTS Coherent Acoustics core substream decoder, including the XCH, XXCH, X96 and XBR core
//! extensions (ETSI TS 102 114).

use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::{BitReader, crc16};
use crate::dsp::{
    Filters, Subbands, SynthState, SynthStateFixed, butterflies_fixed, dmix_scale_inv, dmix_sub,
    dmix_sub_xch, lfe_fir, lfe_fir_fixed, lfe_x96, lfe_x96_fixed, mul16, sub_qmf32_fixed,
    sub_qmf64_fixed,
};
use crate::exss::ExssAsset;
use crate::huff::{CODE_BOOKS, Huffman};
use crate::tables::CHANNELS as CHANNELS_PER_AMODE;
use crate::tables::*;

pub const CHANNELS: usize = 7;
pub const SUBBANDS: usize = 32;
pub const SUBBANDS_X96: usize = 64;
pub const SUBBAND_SAMPLES: usize = 8;
pub const PCMBLOCK_SAMPLES: usize = 32;
pub const LFE_HISTORY: usize = 8;
const ABITS_MAX: usize = 26;
const XXCH_CHANNELS_MAX: usize = 2;
const EXSS_CHSETS_MAX: usize = 4;
const EXSS_CHANNELS_MAX: usize = 8;

/// Number of history samples kept in front of each subband for ADPCM.
pub const ADPCM_COEFFS: usize = 4;
/// Maximum number of PCM blocks (of 32 samples) in a core frame.
const MAX_BLOCKS: usize = 128;
/// Sample stride between subbands in the subband sample buffers.
pub const SUBBAND_STRIDE: usize = ADPCM_COEFFS + MAX_BLOCKS;
/// LFE buffer size: history plus the samples of the longest frame (64x decimation).
const LFE_BUFFER: usize = LFE_HISTORY + MAX_BLOCKS / 2;

pub const SYNC_CORE_BE: u32 = 0x7ffe_8001;
pub const SYNC_CORE_LE: u32 = 0xfe7f_0180;
pub const SYNC_CORE_14B_BE: u32 = 0x1fff_e800;
pub const SYNC_CORE_14B_LE: u32 = 0xff1f_00e8;
pub const SYNC_SUBSTREAM: u32 = 0x6458_2025;
/// Flag of the filtering mode: fixed-point synthesis.
const FILTER_MODE_FIXED: u8 = 0x02;

const SYNC_XCH: u32 = 0x5a5a_5a5a;
const SYNC_XXCH: u32 = 0x4700_4a03;
const SYNC_X96: u32 = 0x1d95_f262;
const SYNC_XBR: u32 = 0x655e_315e;
const SYNC_REV1AUX: u32 = 0x9a11_05a0;

/// Core sampling frequencies indexed by the sample rate code.
const SAMPLE_RATES: [u32; 16] =
    [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000];
/// Source PCM resolution (bits) indexed by the PCMR code.
const BITS_PER_SAMPLE: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];

/// DTS speaker indices (bit positions of the loudspeaker activity mask).
pub mod spkr {
    pub const C: usize = 0;
    pub const L: usize = 1;
    pub const R: usize = 2;
    pub const LS: usize = 3;
    pub const RS: usize = 4;
    pub const LFE1: usize = 5;
    pub const CS: usize = 6;
    pub const LSR: usize = 7;
    pub const RSR: usize = 8;
    pub const LSS: usize = 9;
    pub const RSS: usize = 10;
    pub const COUNT: usize = 28;
}

const MASK_C: u32 = 1 << spkr::C;
const MASK_L: u32 = 1 << spkr::L;
const MASK_R: u32 = 1 << spkr::R;
const MASK_LS: u32 = 1 << spkr::LS;
const MASK_RS: u32 = 1 << spkr::RS;
const MASK_LFE1: u32 = 1 << spkr::LFE1;
const MASK_CS: u32 = 1 << spkr::CS;
const MASK_LSS: u32 = 1 << spkr::LSS;
const MASK_RSS: u32 = 1 << spkr::RSS;

const AMODE_MONO: usize = 0;
const AMODE_STEREO_SUMDIFF: usize = 3;
const AMODE_2F2R: usize = 8;
const AMODE_COUNT: usize = 10;

const PRM_CH_TO_SPKR_MAP: [[i8; 5]; AMODE_COUNT] = [
    [spkr::C as i8, -1, -1, -1, -1],
    [spkr::L as i8, spkr::R as i8, -1, -1, -1],
    [spkr::L as i8, spkr::R as i8, -1, -1, -1],
    [spkr::L as i8, spkr::R as i8, -1, -1, -1],
    [spkr::L as i8, spkr::R as i8, -1, -1, -1],
    [spkr::C as i8, spkr::L as i8, spkr::R as i8, -1, -1],
    [spkr::L as i8, spkr::R as i8, spkr::CS as i8, -1, -1],
    [spkr::C as i8, spkr::L as i8, spkr::R as i8, spkr::CS as i8, -1],
    [spkr::L as i8, spkr::R as i8, spkr::LS as i8, spkr::RS as i8, -1],
    [spkr::C as i8, spkr::L as i8, spkr::R as i8, spkr::LS as i8, spkr::RS as i8],
];

const AUDIO_MODE_CH_MASK: [u32; AMODE_COUNT] = [
    MASK_C,
    MASK_L | MASK_R,
    MASK_L | MASK_R,
    MASK_L | MASK_R,
    MASK_L | MASK_R,
    MASK_L | MASK_R | MASK_C,
    MASK_L | MASK_R | MASK_CS,
    MASK_L | MASK_R | MASK_C | MASK_CS,
    MASK_L | MASK_R | MASK_LS | MASK_RS,
    MASK_L | MASK_R | MASK_C | MASK_LS | MASK_RS,
];

const BLOCK_CODE_NBITS: [u32; 7] = [7, 10, 12, 13, 15, 17, 19];

// Core extension presence flags (`ext_audio_mask`).
const CSS_XXCH: u32 = 0x002;
const CSS_X96: u32 = 0x004;
const CSS_XCH: u32 = 0x008;
const EXSS_XBR: u32 = 0x020;
const EXSS_XXCH: u32 = 0x040;
const EXSS_X96: u32 = 0x080;

// Core extension audio descriptors.
const EXT_AUDIO_XCH: usize = 0;
const EXT_AUDIO_X96: usize = 2;
const EXT_AUDIO_XXCH: usize = 6;

#[derive(Copy, Clone, PartialEq, Eq)]
enum HeaderType {
    Core,
    Xch,
    Xxch,
}

#[inline]
pub(crate) fn clip23(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}

#[inline]
pub(crate) fn norm(a: i64, bits: i32) -> i32 {
    if bits > 0 { (a.wrapping_add(1i64 << (bits - 1)) >> bits) as i32 } else { a as i32 }
}

#[inline]
fn mul(a: i32, b: i32, bits: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), bits)
}

/// Dequantize `input` into `out` using the quantizer step size and scale factor.
fn dequantize(out: &mut [i32], input: &[i32], step_size: i32, scale: i32, residual: bool) {
    let mut step_scale = i64::from(step_size) * i64::from(scale);
    let mut shift = 0;

    // Limit scale factor resolution to 22 bits.
    if step_scale > (1 << 23) {
        shift = (step_scale >> 23).ilog2() as i32 + 1;
        step_scale >>= shift;
    }

    for (o, &x) in out.iter_mut().zip(input) {
        let v = clip23(norm(i64::from(x) * step_scale, 22 - shift));
        *o = if residual { o.wrapping_add(v) } else { v };
    }
}

/// Predict the next subband sample from the four previous ones.
#[inline]
fn adpcm_predict(pred_vq_index: usize, input: &[i32]) -> i32 {
    let coeff = &ADPCM_VB[pred_vq_index];
    let mut pred = 0i64;
    for i in 0..ADPCM_COEFFS {
        pred += i64::from(input[ADPCM_COEFFS - 1 - i]) * i64::from(coeff[i]);
    }
    clip23(norm(pred, 13))
}

/// Information about a filtered frame.
pub struct FrameInfo {
    /// The number of PCM samples per channel.
    pub nsamples: usize,
    /// The sample rate.
    pub sample_rate: u32,
    /// The speaker mask (bit positions are DTS speaker indices) of the channels in `out`.
    pub ch_mask: u32,
}

pub struct CoreDecoder {
    // Frame header.
    crc_present: bool,
    pub npcmblocks: usize,
    pub frame_size: usize,
    audio_mode: usize,
    pub sample_rate: u32,
    bit_rate: u32,
    drc_present: bool,
    ts_present: bool,
    aux_present: bool,
    ext_audio_type: usize,
    ext_audio_present: bool,
    sync_ssf: bool,
    lfe_present: usize,
    predictor_history: bool,
    filter_perfect: bool,
    es_format: bool,
    sumdiff_front: bool,
    sumdiff_surround: bool,

    // Primary coding header.
    nsubframes: usize,
    nchannels: usize,
    ch_mask: u32,
    nsubbands: [usize; CHANNELS],
    subband_vq_start: [usize; CHANNELS],
    joint_intensity_index: [usize; CHANNELS],
    transition_mode_sel: [usize; CHANNELS],
    scale_factor_sel: [usize; CHANNELS],
    bit_allocation_sel: [usize; CHANNELS],
    quant_index_sel: [[usize; CODE_BOOKS]; CHANNELS],
    scale_factor_adj: [[i32; CODE_BOOKS]; CHANNELS],

    // Primary side information.
    nsubsubframes: [usize; 16],
    prediction_mode: [[bool; SUBBANDS_X96]; CHANNELS],
    prediction_vq_index: [[usize; SUBBANDS_X96]; CHANNELS],
    bit_allocation: [[usize; SUBBANDS_X96]; CHANNELS],
    transition_mode: [[[usize; SUBBANDS]; CHANNELS]; 16],
    scale_factors: [[[i32; 2]; SUBBANDS]; CHANNELS],
    joint_scale_sel: [usize; CHANNELS],
    joint_scale_factors: [[i32; SUBBANDS_X96]; CHANNELS],

    // Subband samples: `CHANNELS * SUBBANDS * SUBBAND_STRIDE`, LFE samples and X96 subband samples
    // (`CHANNELS * SUBBANDS_X96 * SUBBAND_STRIDE`).
    subband_samples: Vec<i32>,
    lfe_samples: Vec<i32>,
    x96_subband_samples: Vec<i32>,

    // Extensions.
    ext_audio_mask: u32,
    xch_pos: usize,
    xxch_pos: usize,
    x96_pos: usize,

    xxch_crc_present: bool,
    xxch_mask_nbits: usize,
    xxch_core_mask: u32,
    xxch_spkr_mask: u32,
    xxch_dmix_embedded: bool,
    xxch_dmix_scale_inv: i32,
    xxch_dmix_mask: [u32; XXCH_CHANNELS_MAX],
    xxch_dmix_coeff: [i32; XXCH_CHANNELS_MAX * 32],

    x96_rev_no: u32,
    x96_crc_present: bool,
    x96_nchannels: usize,
    x96_high_res: usize,
    x96_subband_start: usize,
    x96_rand: u32,

    // DSP state.
    filters: Filters,
    synth: [SynthState; CHANNELS],
    lfe_x96_history: f32,
    /// Previous filtering mode, for detecting changes: the X96 (64-band) flag, plus
    /// [`FILTER_MODE_FIXED`] for the fixed-point path.
    filter_mode: u8,
    lfe_tmp: Vec<f32>,
    synth_fixed: [SynthStateFixed; CHANNELS],
    lfe_x96_history_fixed: i32,
    lfe_tmp_fixed: Vec<i32>,

    /// Output PCM, indexed by DTS speaker.
    pub out: Vec<Vec<f32>>,

    /// Fixed-point output PCM of [`CoreDecoder::filter_fixed`], indexed by DTS speaker. Only the
    /// speakers in the channel mask hold valid data, the first `npcmsamples` samples of each.
    /// Samples are 24-bit signed values (not shifted). Buffers only ever grow.
    pub out_fixed: Vec<Vec<i32>>,
    /// Output sample rate of the last fixed-point filtering (1x or 2x the header rate).
    pub output_rate: u32,
    /// Number of PCM samples per channel produced by the last fixed-point filtering.
    pub npcmsamples: usize,
}

impl Default for CoreDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreDecoder {
    pub fn new() -> Self {
        CoreDecoder {
            crc_present: false,
            npcmblocks: 0,
            frame_size: 0,
            audio_mode: 0,
            sample_rate: 0,
            bit_rate: 0,
            drc_present: false,
            ts_present: false,
            aux_present: false,
            ext_audio_type: 0,
            ext_audio_present: false,
            sync_ssf: false,
            lfe_present: 0,
            predictor_history: false,
            filter_perfect: false,
            es_format: false,
            sumdiff_front: false,
            sumdiff_surround: false,
            nsubframes: 0,
            nchannels: 0,
            ch_mask: 0,
            nsubbands: [0; CHANNELS],
            subband_vq_start: [0; CHANNELS],
            joint_intensity_index: [0; CHANNELS],
            transition_mode_sel: [0; CHANNELS],
            scale_factor_sel: [0; CHANNELS],
            bit_allocation_sel: [0; CHANNELS],
            quant_index_sel: [[0; CODE_BOOKS]; CHANNELS],
            scale_factor_adj: [[0; CODE_BOOKS]; CHANNELS],
            nsubsubframes: [0; 16],
            prediction_mode: [[false; SUBBANDS_X96]; CHANNELS],
            prediction_vq_index: [[0; SUBBANDS_X96]; CHANNELS],
            bit_allocation: [[0; SUBBANDS_X96]; CHANNELS],
            transition_mode: [[[0; SUBBANDS]; CHANNELS]; 16],
            scale_factors: [[[0; 2]; SUBBANDS]; CHANNELS],
            joint_scale_sel: [0; CHANNELS],
            joint_scale_factors: [[0; SUBBANDS_X96]; CHANNELS],
            subband_samples: vec![0; CHANNELS * SUBBANDS * SUBBAND_STRIDE],
            lfe_samples: vec![0; LFE_BUFFER],
            x96_subband_samples: Vec::new(),
            ext_audio_mask: 0,
            xch_pos: 0,
            xxch_pos: 0,
            x96_pos: 0,
            xxch_crc_present: false,
            xxch_mask_nbits: 0,
            xxch_core_mask: 0,
            xxch_spkr_mask: 0,
            xxch_dmix_embedded: false,
            xxch_dmix_scale_inv: 0,
            xxch_dmix_mask: [0; XXCH_CHANNELS_MAX],
            xxch_dmix_coeff: [0; XXCH_CHANNELS_MAX * 32],
            x96_rev_no: 0,
            x96_crc_present: false,
            x96_nchannels: 0,
            x96_high_res: 0,
            x96_subband_start: 0,
            x96_rand: 1,
            filters: Filters::default(),
            synth: Default::default(),
            lfe_x96_history: 0.0,
            filter_mode: 0,
            lfe_tmp: Vec::new(),
            synth_fixed: Default::default(),
            lfe_x96_history_fixed: 0,
            lfe_tmp_fixed: Vec::new(),
            out: vec![Vec::new(); spkr::COUNT],
            out_fixed: vec![Vec::new(); spkr::COUNT],
            output_rate: 0,
            npcmsamples: 0,
        }
    }

    /// Reset all state that persists between frames.
    pub fn flush(&mut self) {
        self.erase_adpcm_history();
        self.lfe_samples[..LFE_HISTORY].fill(0);
        if !self.x96_subband_samples.is_empty() {
            self.erase_x96_adpcm_history();
        }
        self.erase_dsp_history();
    }

    /// True if the core of the current frame carries an X96 extension that was decoded.
    pub fn has_x96(&self) -> bool {
        self.ext_audio_mask & (CSS_X96 | EXSS_X96) != 0
    }

    fn erase_dsp_history(&mut self) {
        for s in &mut self.synth {
            s.reset();
        }
        for s in &mut self.synth_fixed {
            s.reset();
        }
        self.lfe_x96_history = 0.0;
        self.lfe_x96_history_fixed = 0;
    }

    /// Handle a change of filtering mode (float/fixed, 32/64 bands), which invalidates the
    /// filter history.
    fn set_filter_mode(&mut self, mode: u8) {
        if self.filter_mode != mode {
            self.erase_dsp_history();
            self.filter_mode = mode;
        }
    }

    fn erase_adpcm_history(&mut self) {
        for ch in 0..CHANNELS {
            for band in 0..SUBBANDS {
                let base = (ch * SUBBANDS + band) * SUBBAND_STRIDE;
                self.subband_samples[base..base + ADPCM_COEFFS].fill(0);
            }
        }
    }

    fn erase_x96_adpcm_history(&mut self) {
        for ch in 0..CHANNELS {
            for band in 0..SUBBANDS_X96 {
                let base = (ch * SUBBANDS_X96 + band) * SUBBAND_STRIDE;
                self.x96_subband_samples[base..base + ADPCM_COEFFS].fill(0);
            }
        }
    }

    #[inline]
    fn sb_base(ch: usize, band: usize) -> usize {
        (ch * SUBBANDS + band) * SUBBAND_STRIDE + ADPCM_COEFFS
    }

    #[inline]
    fn x96_base(ch: usize, band: usize) -> usize {
        (ch * SUBBANDS_X96 + band) * SUBBAND_STRIDE + ADPCM_COEFFS
    }

    // ---------------------------------------------------------------------------------------
    // Frame header (5.3.1)
    // ---------------------------------------------------------------------------------------

    fn parse_frame_header(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        if gb.get(32) != SYNC_CORE_BE {
            return decode_error("dca: invalid core sync word");
        }

        let _normal_frame = gb.get1();

        // Deficit sample count.
        if gb.get(5) + 1 != PCMBLOCK_SAMPLES as u32 {
            return unsupported_error("dca: deficit samples are not supported");
        }

        self.crc_present = gb.get1();

        self.npcmblocks = gb.get(7) as usize + 1;
        if self.npcmblocks & (SUBBAND_SAMPLES - 1) != 0 {
            return decode_error("dca: unsupported number of pcm sample blocks");
        }

        self.frame_size = gb.get(14) as usize + 1;
        if self.frame_size < 96 {
            return decode_error("dca: invalid core frame size");
        }

        self.audio_mode = gb.get(6) as usize;
        if self.audio_mode >= AMODE_COUNT {
            return unsupported_error("dca: unsupported audio channel arrangement");
        }

        let sr_code = gb.get(4) as usize;
        self.sample_rate = SAMPLE_RATES[sr_code];
        if self.sample_rate == 0 {
            return decode_error("dca: invalid core sampling frequency");
        }

        self.bit_rate = BIT_RATES[gb.get(5) as usize];

        if gb.get1() {
            return decode_error("dca: reserved bit set");
        }

        self.drc_present = gb.get1();
        self.ts_present = gb.get1();
        self.aux_present = gb.get1();
        let _hdcd_master = gb.get1();
        self.ext_audio_type = gb.get(3) as usize;
        self.ext_audio_present = gb.get1();
        self.sync_ssf = gb.get1();

        self.lfe_present = gb.get(2) as usize;
        if self.lfe_present == 3 {
            return decode_error("dca: invalid low frequency effects flag");
        }

        self.predictor_history = gb.get1();
        if self.crc_present {
            gb.skip(16);
        }
        self.filter_perfect = gb.get1();
        let _encoder_rev = gb.get(4);
        let _copy_hist = gb.get(2);

        let pcmr_code = gb.get(3) as usize;
        if BITS_PER_SAMPLE[pcmr_code] == 0 {
            return decode_error("dca: invalid source pcm resolution");
        }
        self.es_format = pcmr_code & 1 != 0;

        self.sumdiff_front = gb.get1();
        self.sumdiff_surround = gb.get1();
        let _dn_code = gb.get(4);

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Primary audio coding header (5.3.2)
    // ---------------------------------------------------------------------------------------

    fn parse_coding_header(
        &mut self,
        gb: &mut BitReader<'_>,
        header: HeaderType,
        xch_base: usize,
    ) -> Result<()> {
        let header_pos = gb.pos();
        let mut header_size = 0;

        if gb.left() < 0 {
            return decode_error("dca: truncated coding header");
        }

        match header {
            HeaderType::Core => {
                self.nsubframes = gb.get(4) as usize + 1;

                self.nchannels = gb.get(3) as usize + 1;
                if self.nchannels != usize::from(CHANNELS_PER_AMODE[self.audio_mode]) {
                    return decode_error("dca: invalid number of primary audio channels");
                }

                self.ch_mask = AUDIO_MODE_CH_MASK[self.audio_mode];

                if self.lfe_present != 0 {
                    self.ch_mask |= MASK_LFE1;
                }
            }
            HeaderType::Xch => {
                self.nchannels = usize::from(CHANNELS_PER_AMODE[self.audio_mode]) + 1;
                self.ch_mask |= MASK_CS;
            }
            HeaderType::Xxch => {
                header_size = gb.get(7) as usize + 1;

                // The channel set header CRC is not verified, as in the reference implementation.
                let nchannels = gb.get(3) as usize + 1;
                if nchannels > XXCH_CHANNELS_MAX {
                    return unsupported_error("dca: unsupported number of xxch channels");
                }
                self.nchannels = usize::from(CHANNELS_PER_AMODE[self.audio_mode]) + nchannels;

                let mask = gb.get(self.xxch_mask_nbits as u32 - spkr::CS as u32);
                self.xxch_spkr_mask = mask << spkr::CS;

                if self.xxch_spkr_mask.count_ones() as usize != nchannels {
                    return decode_error("dca: invalid xxch speaker layout mask");
                }

                if self.xxch_core_mask & self.xxch_spkr_mask != 0 {
                    return decode_error("dca: xxch speaker layout mask overlaps with core");
                }

                self.ch_mask = self.xxch_core_mask | self.xxch_spkr_mask;

                // Downmix coefficients present in the stream.
                if gb.get1() {
                    let mut ncoeffs = 0;

                    self.xxch_dmix_embedded = gb.get1();

                    // Downmix scale factor.
                    let index = i64::from(gb.get(6)) * 4
                        - (DMIXTABLE.len() - INV_DMIXTABLE.len()) as i64
                        - 3;
                    if index < 0 || index as usize >= INV_DMIXTABLE.len() {
                        return decode_error("dca: invalid xxch downmix scale index");
                    }
                    self.xxch_dmix_scale_inv = INV_DMIXTABLE[index as usize] as i32;

                    // Downmix channel mapping mask.
                    for ch in 0..nchannels {
                        let mask = gb.get(self.xxch_mask_nbits as u32);
                        if mask & self.xxch_core_mask != mask {
                            return decode_error("dca: invalid xxch downmix channel mapping mask");
                        }
                        self.xxch_dmix_mask[ch] = mask;
                    }

                    // Downmix coefficients.
                    for ch in 0..nchannels {
                        for n in 0..self.xxch_mask_nbits {
                            if self.xxch_dmix_mask[ch] & (1u32 << n) == 0 {
                                continue;
                            }

                            let code = gb.get(7);
                            let sign = (code >> 6) as i32 - 1;
                            let code = (code & 63) as usize;

                            self.xxch_dmix_coeff[ncoeffs] = if code != 0 {
                                let index = code * 4 - 3;
                                if index >= DMIXTABLE.len() {
                                    return decode_error("dca: invalid xxch downmix coefficient");
                                }
                                (i32::from(DMIXTABLE[index]) ^ sign) - sign
                            }
                            else {
                                0
                            };
                            ncoeffs += 1;
                        }
                    }
                }
                else {
                    self.xxch_dmix_embedded = false;
                }
            }
        }

        let nch = self.nchannels;

        // Subband activity count.
        for ch in xch_base..nch {
            self.nsubbands[ch] = gb.get(5) as usize + 2;
            if self.nsubbands[ch] > SUBBANDS {
                return decode_error("dca: invalid subband activity count");
            }
        }

        // High frequency VQ start subband.
        for ch in xch_base..nch {
            self.subband_vq_start[ch] = gb.get(5) as usize + 1;
        }

        // Joint intensity coding index.
        for ch in xch_base..nch {
            let mut n = gb.get(3) as usize;
            if n != 0 && header == HeaderType::Xxch {
                n += xch_base - 1;
            }
            if n > nch {
                return decode_error("dca: invalid joint intensity coding index");
            }
            self.joint_intensity_index[ch] = n;
        }

        // Transient mode code book.
        for ch in xch_base..nch {
            self.transition_mode_sel[ch] = gb.get(2) as usize;
        }

        // Scale factor code book.
        for ch in xch_base..nch {
            self.scale_factor_sel[ch] = gb.get(3) as usize;
            if self.scale_factor_sel[ch] == 7 {
                return decode_error("dca: invalid scale factor code book");
            }
        }

        // Bit allocation quantizer select.
        for ch in xch_base..nch {
            self.bit_allocation_sel[ch] = gb.get(3) as usize;
            if self.bit_allocation_sel[ch] == 7 {
                return decode_error("dca: invalid bit allocation quantizer select");
            }
        }

        // Quantization index codebook select.
        for n in 0..CODE_BOOKS {
            for ch in xch_base..nch {
                self.quant_index_sel[ch][n] = gb.get(u32::from(QUANT_INDEX_SEL_NBITS[n])) as usize;
            }
        }

        // Scale factor adjustment index.
        for n in 0..CODE_BOOKS {
            for ch in xch_base..nch {
                if self.quant_index_sel[ch][n] < usize::from(QUANT_INDEX_GROUP_SIZE[n]) {
                    self.scale_factor_adj[ch][n] = SCALE_FACTOR_ADJ[gb.get(2) as usize] as i32;
                }
            }
        }

        if header == HeaderType::Xxch {
            // Reserved, byte align, CRC16 of channel set header.
            if !gb.seek(header_pos + header_size * 8) {
                return decode_error("dca: read past end of xxch channel set header");
            }
        }
        else if self.crc_present {
            // Audio header CRC check word.
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_scale(
        &self,
        gb: &mut BitReader<'_>,
        scale_index: &mut i32,
        sel: usize,
    ) -> Result<i32> {
        // If a Huffman code was used, the difference of scales was encoded.
        if sel < 5 {
            *scale_index += Huffman::get().scale_factor[sel].decode(gb);
        }
        else {
            *scale_index = gb.get(sel as u32 + 1) as i32;
        }

        // Look up the scale factor from the root square table.
        let idx = *scale_index;
        let scale = if sel > 5 {
            SCALE_FACTOR_QUANT7.get(idx as usize)
        }
        else {
            SCALE_FACTOR_QUANT6.get(idx as usize)
        };

        match scale {
            Some(&s) if idx >= 0 => Ok(s as i32),
            _ => decode_error("dca: invalid scale factor index"),
        }
    }

    fn parse_joint_scale(&self, gb: &mut BitReader<'_>, sel: usize) -> Result<i32> {
        // Absolute value was encoded even when a Huffman code was used.
        let mut scale_index = if sel < 5 {
            Huffman::get().scale_factor[sel].decode(gb)
        }
        else {
            gb.get(sel as u32 + 1) as i32
        };

        // Bias by 64.
        scale_index += 64;

        match JOINT_SCALE_FACTORS.get(scale_index as usize) {
            Some(&s) if scale_index >= 0 => Ok(s as i32),
            _ => decode_error("dca: invalid joint scale factor index"),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Primary audio coding side information (5.4.1)
    // ---------------------------------------------------------------------------------------

    fn parse_subframe_header(
        &mut self,
        gb: &mut BitReader<'_>,
        sf: usize,
        header: HeaderType,
        xch_base: usize,
    ) -> Result<()> {
        let huff = Huffman::get();
        let nch = self.nchannels;

        if gb.left() < 0 {
            return decode_error("dca: truncated subframe header");
        }

        if header == HeaderType::Core {
            // Subsubframe count.
            self.nsubsubframes[sf] = gb.get(2) as usize + 1;

            // Partial subsubframe sample count.
            gb.skip(3);
        }

        // Prediction mode.
        for ch in xch_base..nch {
            for band in 0..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = gb.get1();
            }
        }

        // Prediction coefficients VQ address.
        for ch in xch_base..nch {
            for band in 0..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = gb.get(12) as usize;
                }
            }
        }

        // Bit allocation index.
        for ch in xch_base..nch {
            let sel = self.bit_allocation_sel[ch];

            for band in 0..self.subband_vq_start[ch] {
                let abits = if sel < 5 {
                    huff.bit_allocation[sel].decode(gb) as usize
                }
                else {
                    gb.get(sel as u32 - 1) as usize
                };

                if abits > ABITS_MAX {
                    return decode_error("dca: invalid bit allocation index");
                }

                self.bit_allocation[ch][band] = abits;
            }
        }

        // Transition mode.
        for ch in xch_base..nch {
            // Clear transition mode for all subbands.
            self.transition_mode[sf][ch] = [0; SUBBANDS];

            // Transient possible only if more than one subsubframe.
            if self.nsubsubframes[sf] > 1 {
                let sel = self.transition_mode_sel[ch];
                for band in 0..self.subband_vq_start[ch] {
                    if self.bit_allocation[ch][band] != 0 {
                        self.transition_mode[sf][ch][band] =
                            huff.transition_mode[sel].decode(gb) as usize;
                    }
                }
            }
        }

        // Scale factors.
        for ch in xch_base..nch {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index = 0;

            // Extract scales for subbands up to VQ.
            for band in 0..self.subband_vq_start[ch] {
                if self.bit_allocation[ch][band] != 0 {
                    self.scale_factors[ch][band][0] =
                        self.parse_scale(gb, &mut scale_index, sel)?;
                    if self.transition_mode[sf][ch][band] != 0 {
                        self.scale_factors[ch][band][1] =
                            self.parse_scale(gb, &mut scale_index, sel)?;
                    }
                }
                else {
                    self.scale_factors[ch][band][0] = 0;
                }
            }

            // High frequency VQ subbands.
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                self.scale_factors[ch][band][0] = self.parse_scale(gb, &mut scale_index, sel)?;
            }
        }

        // Joint subband codebook select.
        for ch in xch_base..nch {
            if self.joint_intensity_index[ch] != 0 {
                self.joint_scale_sel[ch] = gb.get(3) as usize;
                if self.joint_scale_sel[ch] == 7 {
                    return decode_error("dca: invalid joint scale factor code book");
                }
            }
        }

        // Scale factors for joint subband coding.
        for ch in xch_base..nch {
            if self.joint_intensity_index[ch] != 0 {
                let src_ch = self.joint_intensity_index[ch] - 1;
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    self.joint_scale_factors[ch][band] = self.parse_joint_scale(gb, sel)?;
                }
            }
        }

        // Dynamic range coefficient.
        if self.drc_present && header == HeaderType::Core {
            gb.skip(8);
        }

        // Side information CRC check word.
        if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_block_codes(
        &self,
        gb: &mut BitReader<'_>,
        audio: &mut [i32],
        abits: usize,
    ) -> Result<()> {
        // Extract block code indices from the bit stream.
        let nbits = BLOCK_CODE_NBITS[abits - 1];
        let mut code1 = gb.get(nbits) as i32;
        let mut code2 = gb.get(nbits) as i32;
        let levels = QUANT_LEVELS[abits] as i32;
        let offset = (levels - 1) / 2;

        // Look up samples from the block code book.
        for a in &mut audio[..SUBBAND_SAMPLES / 2] {
            let div = code1 / levels;
            *a = code1 - div * levels - offset;
            code1 = div;
        }
        for a in &mut audio[SUBBAND_SAMPLES / 2..SUBBAND_SAMPLES] {
            let div = code2 / levels;
            *a = code2 - div * levels - offset;
            code2 = div;
        }

        if code1 | code2 != 0 {
            return decode_error("dca: failed to decode block code(s)");
        }

        Ok(())
    }

    /// Extract the samples of one subband subsubframe. Returns `true` if the samples were Huffman
    /// coded (requiring scale factor adjustment).
    fn extract_audio(
        &self,
        gb: &mut BitReader<'_>,
        audio: &mut [i32],
        abits: usize,
        ch: usize,
    ) -> Result<bool> {
        debug_assert!(abits <= ABITS_MAX);

        if abits == 0 {
            // No bits allocated.
            audio[..SUBBAND_SAMPLES].fill(0);
            return Ok(false);
        }

        if abits <= CODE_BOOKS {
            let sel = self.quant_index_sel[ch][abits - 1];
            if sel < usize::from(QUANT_INDEX_GROUP_SIZE[abits - 1]) {
                // Huffman codes.
                let table = &Huffman::get().quant_index[abits - 1][sel];
                for a in &mut audio[..SUBBAND_SAMPLES] {
                    *a = table.decode(gb);
                }
                return Ok(true);
            }
            if abits <= 7 {
                // Block codes.
                self.parse_block_codes(gb, audio, abits)?;
                return Ok(false);
            }
        }

        // No further encoding.
        for a in &mut audio[..SUBBAND_SAMPLES] {
            *a = gb.get_signed(abits as u32 - 3);
        }
        Ok(false)
    }

    /// Inverse ADPCM on the subband sample array `samples` with per-channel layout given by
    /// `base(band)`.
    fn inverse_adpcm(
        samples: &mut [i32],
        base: impl Fn(usize) -> usize,
        vq_index: &[usize],
        prediction_mode: &[bool],
        sb_start: usize,
        sb_end: usize,
        ofs: usize,
        len: usize,
    ) {
        for band in sb_start..sb_end {
            if prediction_mode[band] {
                let pred_id = vq_index[band];
                let start = base(band) + ofs;
                for j in 0..len {
                    let x = adpcm_predict(pred_id, &samples[start + j - ADPCM_COEFFS..start + j]);
                    samples[start + j] = clip23(samples[start + j].wrapping_add(x));
                }
            }
        }
    }

    /// Joint subband coding: derive the subbands `sb_start..sb_end` of `ch` from `src_ch`.
    fn decode_joint(
        samples: &mut [i32],
        base: impl Fn(usize, usize) -> usize,
        ch: usize,
        src_ch: usize,
        scale_factors: &[i32],
        sb_start: usize,
        sb_end: usize,
        ofs: usize,
        len: usize,
    ) {
        for band in sb_start..sb_end {
            let scale = scale_factors[band];
            let dst = base(ch, band) + ofs;
            let src = base(src_ch, band) + ofs;
            for j in 0..len {
                samples[dst + j] = clip23(mul(samples[src + j], scale, 17));
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Primary audio data arrays (5.5)
    // ---------------------------------------------------------------------------------------

    fn parse_subframe_audio(
        &mut self,
        gb: &mut BitReader<'_>,
        sf: usize,
        header: HeaderType,
        xch_base: usize,
        sub_pos: &mut usize,
        lfe_pos: &mut usize,
    ) -> Result<()> {
        let mut audio = [0i32; 16];
        let nch = self.nchannels;
        let nsubsubframes = self.nsubsubframes[sf];

        // Check number of subband samples in this subframe.
        let nsamples = nsubsubframes * SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return decode_error("dca: subband sample buffer overflow");
        }

        if gb.left() < 0 {
            return decode_error("dca: truncated subframe audio");
        }

        // VQ encoded subbands.
        for ch in xch_base..nch {
            let mut vq_index = [0usize; SUBBANDS];

            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                // Extract the VQ address from the bit stream.
                vq_index[band] = gb.get(10) as usize;
            }

            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                let coeff = &HIGH_FREQ_VQ[vq_index[band]];
                let scale = self.scale_factors[ch][band][0];
                let start = Self::sb_base(ch, band) + *sub_pos;
                for j in 0..nsamples {
                    self.subband_samples[start + j] =
                        clip23(i32::from(coeff[j]).wrapping_mul(scale).wrapping_add(1 << 3) >> 4);
                }
            }
        }

        // Low frequency effect data.
        if self.lfe_present != 0 && header == HeaderType::Core {
            // Determine number of LFE samples in this subframe.
            let nlfesamples = 2 * self.lfe_present * nsubsubframes;

            // Extract LFE samples from the bit stream.
            for a in &mut audio[..nlfesamples] {
                *a = gb.get_signed(8);
            }

            // Extract scale factor index from the bit stream.
            let index = gb.get(8) as usize;
            if index >= SCALE_FACTOR_QUANT7.len() {
                return decode_error("dca: invalid lfe scale factor index");
            }

            // Look up the 7-bit root square quantization table.
            let mut scale = SCALE_FACTOR_QUANT7[index] as i32;

            // Account for quantizer step size which is 0.035.
            scale = mul(4697620 /* 0.035 * (1 << 27) */, scale, 23);

            // Scale and take the LFE samples.
            for (n, &a) in audio[..nlfesamples].iter().enumerate() {
                self.lfe_samples[*lfe_pos + n] = clip23(a.wrapping_mul(scale) >> 4);
            }

            // Advance LFE sample pointer for the next subframe.
            *lfe_pos += nlfesamples;
        }

        // Audio data.
        let mut ofs = *sub_pos;
        for ssf in 0..nsubsubframes {
            for ch in xch_base..nch {
                if gb.left() < 0 {
                    return decode_error("dca: truncated subframe audio");
                }

                // Not high frequency VQ subbands.
                for band in 0..self.subband_vq_start[ch] {
                    let abits = self.bit_allocation[ch][band];

                    // Extract bits from the bit stream.
                    let huffman = self.extract_audio(gb, &mut audio, abits, ch)?;

                    // Select quantization step size table and look up quantization step size.
                    let step_size = if self.bit_rate == 3 {
                        LOSSLESS_QUANT[abits] as i32
                    }
                    else {
                        LOSSY_QUANT[abits] as i32
                    };

                    // Identify transient location.
                    let trans_ssf = self.transition_mode[sf][ch][band];

                    // Determine proper scale factor.
                    let mut scale = if trans_ssf == 0 || ssf < trans_ssf {
                        self.scale_factors[ch][band][0]
                    }
                    else {
                        self.scale_factors[ch][band][1]
                    };

                    // Adjust scale factor when SEL indicates Huffman code.
                    if huffman {
                        let adj = i64::from(self.scale_factor_adj[ch][abits - 1]);
                        scale = clip23(((adj * i64::from(scale)) >> 22) as i32);
                    }

                    let start = Self::sb_base(ch, band) + ofs;
                    dequantize(
                        &mut self.subband_samples[start..start + SUBBAND_SAMPLES],
                        &audio,
                        step_size,
                        scale,
                        false,
                    );
                }
            }

            // DSYNC.
            if (ssf == nsubsubframes - 1 || self.sync_ssf) && gb.get(16) != 0xffff {
                return decode_error("dca: dsync check failed");
            }

            ofs += SUBBAND_SAMPLES;
        }

        // Inverse ADPCM.
        for ch in xch_base..nch {
            Self::inverse_adpcm(
                &mut self.subband_samples,
                |band| Self::sb_base(ch, band),
                &self.prediction_vq_index[ch],
                &self.prediction_mode[ch],
                0,
                self.nsubbands[ch],
                *sub_pos,
                nsamples,
            );
        }

        // Joint subband coding.
        for ch in xch_base..nch {
            if self.joint_intensity_index[ch] != 0 {
                let src_ch = self.joint_intensity_index[ch] - 1;
                Self::decode_joint(
                    &mut self.subband_samples,
                    Self::sb_base,
                    ch,
                    src_ch,
                    &self.joint_scale_factors[ch],
                    self.nsubbands[ch],
                    self.nsubbands[src_ch],
                    *sub_pos,
                    nsamples,
                );
            }
        }

        // Advance subband sample pointer for the next subframe.
        *sub_pos = ofs;
        Ok(())
    }

    fn parse_frame_data(
        &mut self,
        gb: &mut BitReader<'_>,
        header: HeaderType,
        xch_base: usize,
    ) -> Result<()> {
        self.parse_coding_header(gb, header, xch_base)?;

        let mut sub_pos = 0;
        let mut lfe_pos = LFE_HISTORY;

        for sf in 0..self.nsubframes {
            self.parse_subframe_header(gb, sf, header, xch_base)?;
            self.parse_subframe_audio(gb, sf, header, xch_base, &mut sub_pos, &mut lfe_pos)?;
        }

        for ch in xch_base..self.nchannels {
            // Determine number of active subbands for this channel.
            let mut nsubbands = self.nsubbands[ch];
            if self.joint_intensity_index[ch] != 0 {
                nsubbands = nsubbands.max(self.nsubbands[self.joint_intensity_index[ch] - 1]);
            }

            // Update history for ADPCM.
            for band in 0..nsubbands {
                let base = Self::sb_base(ch, band);
                self.subband_samples.copy_within(
                    base + self.npcmblocks - ADPCM_COEFFS..base + self.npcmblocks,
                    base - ADPCM_COEFFS,
                );
            }

            // Clear inactive subbands.
            for band in nsubbands..SUBBANDS {
                let base = Self::sb_base(ch, band) - ADPCM_COEFFS;
                self.subband_samples[base..base + ADPCM_COEFFS + self.npcmblocks].fill(0);
            }
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // XCH
    // ---------------------------------------------------------------------------------------

    fn parse_xch_frame(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        if self.ch_mask & MASK_CS != 0 {
            return decode_error("dca: xch with cs speaker already present");
        }

        let base = self.nchannels;
        self.parse_frame_data(gb, HeaderType::Xch, base)?;

        // Seek to the end of core frame, don't trust XCH frame size.
        if !gb.seek(self.frame_size * 8) {
            return decode_error("dca: read past end of xch frame");
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // XXCH
    // ---------------------------------------------------------------------------------------

    fn parse_xxch_frame(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        let header_pos = gb.pos();

        // XXCH sync word.
        if gb.get(32) != SYNC_XXCH {
            return decode_error("dca: invalid xxch sync word");
        }

        // XXCH frame header length.
        let header_size = gb.get(6) as usize + 1;

        // CRC presence flag for channel set header.
        self.xxch_crc_present = gb.get1();

        // Number of bits for loudspeaker mask.
        self.xxch_mask_nbits = gb.get(5) as usize + 1;
        if self.xxch_mask_nbits <= spkr::CS {
            return decode_error("dca: invalid number of bits for xxch speaker mask");
        }

        // Number of channel sets.
        let xxch_nchsets = gb.get(2) + 1;
        if xxch_nchsets > 1 {
            return unsupported_error("dca: multiple xxch channel sets");
        }

        // Channel set 0 data byte size.
        let xxch_frame_size = gb.get(14) as usize + 1;

        // Core loudspeaker activity mask.
        self.xxch_core_mask = gb.get(self.xxch_mask_nbits as u32);

        // Validate the core mask.
        let mut mask = self.ch_mask;

        if mask & MASK_LS != 0 && self.xxch_core_mask & MASK_LSS != 0 {
            mask = (mask & !MASK_LS) | MASK_LSS;
        }

        if mask & MASK_RS != 0 && self.xxch_core_mask & MASK_RSS != 0 {
            mask = (mask & !MASK_RS) | MASK_RSS;
        }

        if mask != self.xxch_core_mask {
            return decode_error("dca: xxch core speaker activity mask disagrees with core");
        }

        // Reserved, byte align, CRC16 of XXCH frame header.
        if !gb.seek(header_pos + header_size * 8) {
            return decode_error("dca: read past end of xxch frame header");
        }

        // Parse XXCH channel set 0.
        let base = self.nchannels;
        self.parse_frame_data(gb, HeaderType::Xxch, base)?;

        if !gb.seek(header_pos + header_size * 8 + xxch_frame_size * 8) {
            return decode_error("dca: read past end of xxch channel set");
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // XBR
    // ---------------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn parse_xbr_subframe(
        &mut self,
        gb: &mut BitReader<'_>,
        xbr_base_ch: usize,
        xbr_nchannels: usize,
        xbr_nsubbands: &[usize],
        xbr_transition_mode: bool,
        sf: usize,
        sub_pos: &mut usize,
    ) -> Result<()> {
        let mut xbr_nabits = [0u32; CHANNELS];
        let mut xbr_bit_allocation = [[0usize; SUBBANDS]; CHANNELS];
        let mut xbr_scale_nbits = [0u32; CHANNELS];
        let mut xbr_scale_factors = [[[0i32; 2]; SUBBANDS]; CHANNELS];

        // Check number of subband samples in this subframe.
        if *sub_pos + self.nsubsubframes[sf] * SUBBAND_SAMPLES > self.npcmblocks {
            return decode_error("dca: subband sample buffer overflow");
        }

        if gb.left() < 0 {
            return decode_error("dca: truncated xbr subframe");
        }

        // Number of bits for XBR bit allocation index.
        for ch in xbr_base_ch..xbr_nchannels {
            xbr_nabits[ch] = gb.get(2) + 2;
        }

        // XBR bit allocation index.
        for ch in xbr_base_ch..xbr_nchannels {
            for band in 0..xbr_nsubbands[ch] {
                let abits = gb.get(xbr_nabits[ch]) as usize;
                if abits > ABITS_MAX {
                    return decode_error("dca: invalid xbr bit allocation index");
                }
                xbr_bit_allocation[ch][band] = abits;
            }
        }

        // Number of bits for scale indices.
        for ch in xbr_base_ch..xbr_nchannels {
            xbr_scale_nbits[ch] = gb.get(3);
            if xbr_scale_nbits[ch] == 0 {
                return decode_error("dca: invalid number of bits for xbr scale factor index");
            }
        }

        // XBR scale factors.
        for ch in xbr_base_ch..xbr_nchannels {
            // Select the root square table.
            let table: &[u32] = if self.scale_factor_sel[ch] > 5 {
                &SCALE_FACTOR_QUANT7
            }
            else {
                &SCALE_FACTOR_QUANT6
            };

            // Parse scale factor indices and look up scale factors from the root square table.
            for band in 0..xbr_nsubbands[ch] {
                if xbr_bit_allocation[ch][band] != 0 {
                    let scale_index = gb.get(xbr_scale_nbits[ch]) as usize;
                    let Some(&s) = table.get(scale_index)
                    else {
                        return decode_error("dca: invalid xbr scale factor index");
                    };
                    xbr_scale_factors[ch][band][0] = s as i32;

                    if xbr_transition_mode && self.transition_mode[sf][ch][band] != 0 {
                        let scale_index = gb.get(xbr_scale_nbits[ch]) as usize;
                        let Some(&s) = table.get(scale_index)
                        else {
                            return decode_error("dca: invalid xbr scale factor index");
                        };
                        xbr_scale_factors[ch][band][1] = s as i32;
                    }
                }
            }
        }

        // Audio data.
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in xbr_base_ch..xbr_nchannels {
                if gb.left() < 0 {
                    return decode_error("dca: truncated xbr subframe");
                }

                for band in 0..xbr_nsubbands[ch] {
                    let abits = xbr_bit_allocation[ch][band];
                    let mut audio = [0i32; SUBBAND_SAMPLES];

                    // Extract bits from the bit stream.
                    if abits > 7 {
                        // No further encoding.
                        for a in &mut audio {
                            *a = gb.get_signed(abits as u32 - 3);
                        }
                    }
                    else if abits > 0 {
                        // Block codes.
                        self.parse_block_codes(gb, &mut audio, abits)?;
                    }
                    else {
                        // No bits allocated.
                        continue;
                    }

                    // Look up quantization step size.
                    let step_size = LOSSLESS_QUANT[abits] as i32;

                    // Identify transient location.
                    let trans_ssf =
                        if xbr_transition_mode { self.transition_mode[sf][ch][band] } else { 0 };

                    // Determine proper scale factor.
                    let scale = if trans_ssf == 0 || ssf < trans_ssf {
                        xbr_scale_factors[ch][band][0]
                    }
                    else {
                        xbr_scale_factors[ch][band][1]
                    };

                    let start = Self::sb_base(ch, band) + ofs;
                    dequantize(
                        &mut self.subband_samples[start..start + SUBBAND_SAMPLES],
                        &audio,
                        step_size,
                        scale,
                        true,
                    );
                }
            }

            // DSYNC.
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && gb.get(16) != 0xffff {
                return decode_error("dca: xbr dsync check failed");
            }

            ofs += SUBBAND_SAMPLES;
        }

        // Advance subband sample pointer for the next subframe.
        *sub_pos = ofs;
        Ok(())
    }

    fn parse_xbr_frame(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        let mut xbr_frame_size = [0usize; EXSS_CHSETS_MAX];
        let mut xbr_nchannels = [0usize; EXSS_CHSETS_MAX];
        let mut xbr_nsubbands = [0usize; EXSS_CHSETS_MAX * EXSS_CHANNELS_MAX];

        let mut header_pos = gb.pos();

        // XBR sync word.
        if gb.get(32) != SYNC_XBR {
            return decode_error("dca: invalid xbr sync word");
        }

        // XBR frame header length.
        let header_size = gb.get(6) as usize + 1;

        // Number of channel sets.
        let xbr_nchsets = gb.get(2) as usize + 1;

        // Channel set data byte size.
        for size in &mut xbr_frame_size[..xbr_nchsets] {
            *size = gb.get(14) as usize + 1;
        }

        // Transition mode flag.
        let xbr_transition_mode = gb.get1();

        // Channel set headers.
        let mut ch2 = 0;
        for i in 0..xbr_nchsets {
            xbr_nchannels[i] = gb.get(3) as usize + 1;
            let xbr_band_nbits = gb.get(2) + 5;
            for _ in 0..xbr_nchannels[i] {
                xbr_nsubbands[ch2] = gb.get(xbr_band_nbits) as usize + 1;
                if xbr_nsubbands[ch2] > SUBBANDS {
                    return decode_error("dca: invalid number of active xbr subbands");
                }
                ch2 += 1;
            }
        }

        // Reserved, byte align, CRC16 of XBR frame header.
        if !gb.seek(header_pos + header_size * 8) {
            return decode_error("dca: read past end of xbr frame header");
        }

        // Channel set data.
        let mut xbr_base_ch = 0;
        for i in 0..xbr_nchsets {
            header_pos = gb.pos();

            if xbr_base_ch + xbr_nchannels[i] <= self.nchannels {
                let mut sub_pos = 0;
                for sf in 0..self.nsubframes {
                    self.parse_xbr_subframe(
                        gb,
                        xbr_base_ch,
                        xbr_base_ch + xbr_nchannels[i],
                        &xbr_nsubbands,
                        xbr_transition_mode,
                        sf,
                        &mut sub_pos,
                    )?;
                }
            }

            xbr_base_ch += xbr_nchannels[i];

            if !gb.seek(header_pos + xbr_frame_size[i] * 8) {
                return decode_error("dca: read past end of xbr channel set");
            }
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // X96
    // ---------------------------------------------------------------------------------------

    /// Modified ISO/IEC 9899 linear congruential generator. Returns a pseudorandom integer in
    /// the range [-2^30, 2^30 - 1].
    fn rand_x96(&mut self) -> i32 {
        self.x96_rand = self.x96_rand.wrapping_mul(1103515245).wrapping_add(12345);
        (self.x96_rand & 0x7fff_ffff) as i32 - 0x4000_0000
    }

    fn parse_x96_subframe_audio(
        &mut self,
        gb: &mut BitReader<'_>,
        sf: usize,
        xch_base: usize,
        sub_pos: &mut usize,
    ) -> Result<()> {
        let nsubsubframes = self.nsubsubframes[sf];
        let nsamples = nsubsubframes * SUBBAND_SAMPLES;
        let x96_nch = self.x96_nchannels;

        // Check number of subband samples in this subframe.
        if *sub_pos + nsamples > self.npcmblocks {
            return decode_error("dca: subband sample buffer overflow");
        }

        if gb.left() < 0 {
            return decode_error("dca: truncated x96 subframe");
        }

        // VQ encoded or unallocated subbands.
        for ch in xch_base..x96_nch {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                // Get the sample pointer and scale factor.
                let start = Self::x96_base(ch, band) + *sub_pos;
                let scale = self.scale_factors[ch][band >> 1][band & 1];

                match self.bit_allocation[ch][band] {
                    0 => {
                        // No bits allocated for subband.
                        if scale <= 1 {
                            self.x96_subband_samples[start..start + nsamples].fill(0);
                        }
                        else {
                            for n in 0..nsamples {
                                // Generate scaled random samples.
                                let r = self.rand_x96();
                                self.x96_subband_samples[start + n] = mul(r, scale, 31);
                            }
                        }
                    }
                    1 => {
                        // VQ encoded subband.
                        for ssf in 0..nsubsubframes.div_ceil(2) {
                            // Extract the VQ address from the bit stream and look up the VQ code
                            // book for up to 16 subband samples.
                            let vq = &HIGH_FREQ_VQ[gb.get(10) as usize];
                            // Scale and take the samples.
                            for n in 0..(nsamples - ssf * 16).min(16) {
                                self.x96_subband_samples[start + ssf * 16 + n] = clip23(
                                    i32::from(vq[n]).wrapping_mul(scale).wrapping_add(1 << 3) >> 4,
                                );
                            }
                        }
                    }
                    _ => (),
                }
            }
        }

        // Audio data.
        let mut ofs = *sub_pos;
        for ssf in 0..nsubsubframes {
            for ch in xch_base..x96_nch {
                if gb.left() < 0 {
                    return decode_error("dca: truncated x96 subframe");
                }

                for band in self.x96_subband_start..self.nsubbands[ch] {
                    // Not VQ encoded or unallocated subbands.
                    if self.bit_allocation[ch][band] < 2 {
                        continue;
                    }
                    let abits = self.bit_allocation[ch][band] - 1;
                    let mut audio = [0i32; 16];

                    // Extract bits from the bit stream.
                    self.extract_audio(gb, &mut audio, abits, ch)?;

                    // Select quantization step size table and look up quantization step size.
                    let step_size = if self.bit_rate == 3 {
                        LOSSLESS_QUANT[abits] as i32
                    }
                    else {
                        LOSSY_QUANT[abits] as i32
                    };

                    // Get the scale factor.
                    let scale = self.scale_factors[ch][band >> 1][band & 1];

                    let start = Self::x96_base(ch, band) + ofs;
                    dequantize(
                        &mut self.x96_subband_samples[start..start + SUBBAND_SAMPLES],
                        &audio,
                        step_size,
                        scale,
                        false,
                    );
                }
            }

            // DSYNC.
            if (ssf == nsubsubframes - 1 || self.sync_ssf) && gb.get(16) != 0xffff {
                return decode_error("dca: x96 dsync check failed");
            }

            ofs += SUBBAND_SAMPLES;
        }

        // Inverse ADPCM.
        for ch in xch_base..x96_nch {
            Self::inverse_adpcm(
                &mut self.x96_subband_samples,
                |band| Self::x96_base(ch, band),
                &self.prediction_vq_index[ch],
                &self.prediction_mode[ch],
                self.x96_subband_start,
                self.nsubbands[ch],
                *sub_pos,
                nsamples,
            );
        }

        // Joint subband coding.
        for ch in xch_base..x96_nch {
            if self.joint_intensity_index[ch] != 0 {
                let src_ch = self.joint_intensity_index[ch] - 1;
                Self::decode_joint(
                    &mut self.x96_subband_samples,
                    Self::x96_base,
                    ch,
                    src_ch,
                    &self.joint_scale_factors[ch],
                    self.nsubbands[ch],
                    self.nsubbands[src_ch],
                    *sub_pos,
                    nsamples,
                );
            }
        }

        // Advance subband sample pointer for the next subframe.
        *sub_pos = ofs;
        Ok(())
    }

    fn alloc_x96_sample_buffer(&mut self) {
        if self.x96_subband_samples.is_empty() {
            self.x96_subband_samples = vec![0; CHANNELS * SUBBANDS_X96 * SUBBAND_STRIDE];
        }

        if !self.predictor_history {
            self.erase_x96_adpcm_history();
        }
    }

    fn parse_x96_subframe_header(&mut self, gb: &mut BitReader<'_>, xch_base: usize) -> Result<()> {
        let huff = Huffman::get();
        let x96_nch = self.x96_nchannels;

        if gb.left() < 0 {
            return decode_error("dca: truncated x96 subframe header");
        }

        // Prediction mode.
        for ch in xch_base..x96_nch {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = gb.get1();
            }
        }

        // Prediction coefficients VQ address.
        for ch in xch_base..x96_nch {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = gb.get(12) as usize;
                }
            }
        }

        // Bit allocation index.
        for ch in xch_base..x96_nch {
            let sel = self.bit_allocation_sel[ch];
            let mut abits = 0i32;

            for band in self.x96_subband_start..self.nsubbands[ch] {
                // If a Huffman code was used, the difference of abits was encoded.
                if sel < 7 {
                    abits += huff.quant_index[5 + 2 * self.x96_high_res][sel].decode(gb);
                }
                else {
                    abits = gb.get(3 + self.x96_high_res as u32) as i32;
                }

                if abits < 0 || abits > 7 + 8 * self.x96_high_res as i32 {
                    return decode_error("dca: invalid x96 bit allocation index");
                }

                self.bit_allocation[ch][band] = abits as usize;
            }
        }

        // Scale factors.
        for ch in xch_base..x96_nch {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index = 0;

            // Extract scales for subbands which are transmitted even for unallocated subbands.
            for band in self.x96_subband_start..self.nsubbands[ch] {
                self.scale_factors[ch][band >> 1][band & 1] =
                    self.parse_scale(gb, &mut scale_index, sel)?;
            }
        }

        // Joint subband codebook select.
        for ch in xch_base..x96_nch {
            if self.joint_intensity_index[ch] != 0 {
                self.joint_scale_sel[ch] = gb.get(3) as usize;
                if self.joint_scale_sel[ch] == 7 {
                    return decode_error("dca: invalid x96 joint scale factor code book");
                }
            }
        }

        // Scale factors for joint subband coding.
        for ch in xch_base..x96_nch {
            if self.joint_intensity_index[ch] != 0 {
                let src_ch = self.joint_intensity_index[ch] - 1;
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    self.joint_scale_factors[ch][band] = self.parse_joint_scale(gb, sel)?;
                }
            }
        }

        // Side information CRC check word.
        if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_x96_coding_header(
        &mut self,
        gb: &mut BitReader<'_>,
        exss: bool,
        xch_base: usize,
    ) -> Result<()> {
        let header_pos = gb.pos();
        let mut header_size = 0;
        let x96_nch = self.x96_nchannels;

        if gb.left() < 0 {
            return decode_error("dca: truncated x96 coding header");
        }

        if exss {
            // Channel set header length.
            header_size = gb.get(7) as usize + 1;
        }

        // High resolution flag.
        self.x96_high_res = usize::from(gb.get1());

        // First encoded subband.
        if self.x96_rev_no < 8 {
            self.x96_subband_start = gb.get(5) as usize;
            if self.x96_subband_start > 27 {
                return decode_error("dca: invalid x96 subband start index");
            }
        }
        else {
            self.x96_subband_start = SUBBANDS;
        }

        // Subband activity count.
        for ch in xch_base..x96_nch {
            self.nsubbands[ch] = gb.get(6) as usize + 1;
            if self.nsubbands[ch] < SUBBANDS {
                return decode_error("dca: invalid x96 subband activity count");
            }
        }

        // Joint intensity coding index.
        for ch in xch_base..x96_nch {
            let mut n = gb.get(3) as usize;
            if n != 0 && xch_base != 0 {
                n += xch_base - 1;
            }
            if n > x96_nch {
                return decode_error("dca: invalid x96 joint intensity coding index");
            }
            self.joint_intensity_index[ch] = n;
        }

        // Scale factor code book.
        for ch in xch_base..x96_nch {
            self.scale_factor_sel[ch] = gb.get(3) as usize;
            if self.scale_factor_sel[ch] >= 6 {
                return decode_error("dca: invalid x96 scale factor code book");
            }
        }

        // Bit allocation quantizer select.
        for ch in xch_base..x96_nch {
            self.bit_allocation_sel[ch] = gb.get(3) as usize;
        }

        // Quantization index codebook select.
        for n in 0..6 + 4 * self.x96_high_res {
            for ch in xch_base..x96_nch {
                self.quant_index_sel[ch][n] = gb.get(u32::from(QUANT_INDEX_SEL_NBITS[n])) as usize;
            }
        }

        if exss {
            // Reserved, byte align, CRC16 of channel set header.
            if !gb.seek(header_pos + header_size * 8) {
                return decode_error("dca: read past end of x96 channel set header");
            }
        }
        else if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_x96_frame_data(
        &mut self,
        gb: &mut BitReader<'_>,
        exss: bool,
        xch_base: usize,
    ) -> Result<()> {
        self.parse_x96_coding_header(gb, exss, xch_base)?;

        let mut sub_pos = 0;
        for sf in 0..self.nsubframes {
            self.parse_x96_subframe_header(gb, xch_base)?;
            self.parse_x96_subframe_audio(gb, sf, xch_base, &mut sub_pos)?;
        }

        for ch in xch_base..self.x96_nchannels {
            // Determine number of active subbands for this channel.
            let mut nsubbands = self.nsubbands[ch];
            if self.joint_intensity_index[ch] != 0 {
                nsubbands = nsubbands.max(self.nsubbands[self.joint_intensity_index[ch] - 1]);
            }

            // Update history for ADPCM and clear inactive subbands.
            for band in 0..SUBBANDS_X96 {
                let base = Self::x96_base(ch, band);
                if band >= self.x96_subband_start && band < nsubbands {
                    self.x96_subband_samples.copy_within(
                        base + self.npcmblocks - ADPCM_COEFFS..base + self.npcmblocks,
                        base - ADPCM_COEFFS,
                    );
                }
                else {
                    self.x96_subband_samples[base - ADPCM_COEFFS..base + self.npcmblocks].fill(0);
                }
            }
        }

        Ok(())
    }

    fn parse_x96_frame(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        // Revision number.
        self.x96_rev_no = gb.get(4);
        if self.x96_rev_no < 1 || self.x96_rev_no > 8 {
            return decode_error("dca: invalid x96 revision");
        }

        self.x96_crc_present = false;
        self.x96_nchannels = self.nchannels;

        self.alloc_x96_sample_buffer();

        self.parse_x96_frame_data(gb, false, 0)?;

        // Seek to the end of core frame.
        if !gb.seek(self.frame_size * 8) {
            return decode_error("dca: read past end of x96 frame");
        }

        Ok(())
    }

    fn parse_x96_frame_exss(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        let mut x96_frame_size = [0usize; EXSS_CHSETS_MAX];
        let mut x96_nchannels = [0usize; EXSS_CHSETS_MAX];

        let mut header_pos = gb.pos();

        // X96 sync word.
        if gb.get(32) != SYNC_X96 {
            return decode_error("dca: invalid x96 sync word");
        }

        // X96 frame header length.
        let header_size = gb.get(6) as usize + 1;

        // Revision number.
        self.x96_rev_no = gb.get(4);
        if self.x96_rev_no < 1 || self.x96_rev_no > 8 {
            return decode_error("dca: invalid x96 revision");
        }

        // CRC presence flag for channel set header.
        self.x96_crc_present = gb.get1();

        // Number of channel sets.
        let x96_nchsets = gb.get(2) as usize + 1;

        // Channel set data byte size.
        for size in &mut x96_frame_size[..x96_nchsets] {
            *size = gb.get(12) as usize + 1;
        }

        // Number of channels in channel set.
        for n in &mut x96_nchannels[..x96_nchsets] {
            *n = gb.get(3) as usize + 1;
        }

        // Reserved, byte align, CRC16 of X96 frame header.
        if !gb.seek(header_pos + header_size * 8) {
            return decode_error("dca: read past end of x96 frame header");
        }

        self.alloc_x96_sample_buffer();

        // Channel set data.
        self.x96_nchannels = 0;
        let mut x96_base_ch = 0;
        for i in 0..x96_nchsets {
            header_pos = gb.pos();

            if x96_base_ch + x96_nchannels[i] <= self.nchannels {
                self.x96_nchannels = x96_base_ch + x96_nchannels[i];
                self.parse_x96_frame_data(gb, true, x96_base_ch)?;
            }

            x96_base_ch += x96_nchannels[i];

            if !gb.seek(header_pos + x96_frame_size[i] * 8) {
                return decode_error("dca: read past end of x96 channel set");
            }
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Optional information, extension search and frame parsing entry points
    // ---------------------------------------------------------------------------------------

    fn parse_aux_data(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        if gb.left() < 0 {
            return decode_error("dca: truncated auxiliary data");
        }

        // Auxiliary data byte count (can't be trusted).
        gb.skip(6);

        // 4-byte align.
        gb.skip(gb.pos().wrapping_neg() & 31);

        // Auxiliary data sync word.
        if gb.get(32) != SYNC_REV1AUX {
            return decode_error("dca: invalid auxiliary data sync word");
        }

        // Auxiliary decode time stamp flag.
        if gb.get1() {
            gb.skip(47);
        }

        // Auxiliary dynamic downmix flag. The embedded primary downmix coefficients are not used
        // by this decoder (it never downmixes), but must be consumed.
        if gb.get1() {
            // Auxiliary primary channel downmix type.
            let dmix_type = gb.get(3) as usize;
            if dmix_type >= 7 {
                return decode_error("dca: invalid primary channel set downmix type");
            }

            // Size of downmix coefficients matrix.
            let m = usize::from(DMIX_PRIMARY_NCH[dmix_type]);
            let n = usize::from(CHANNELS_PER_AMODE[self.audio_mode])
                + usize::from(self.lfe_present != 0);

            // Dynamic downmix code coefficients.
            for _ in 0..m * n {
                let code = gb.get(9);
                if (code & 0xff) as usize >= DMIXTABLE.len() {
                    return decode_error("dca: invalid downmix coefficient index");
                }
            }
        }

        // Byte align.
        gb.skip(gb.pos().wrapping_neg() & 7);

        // CRC16 of auxiliary data.
        gb.skip(16);

        Ok(())
    }

    fn parse_optional_info(&mut self, gb: &mut BitReader<'_>) {
        // Time code stamp.
        if self.ts_present {
            gb.skip(32);
        }

        // Auxiliary data. Errors are not fatal.
        if self.aux_present {
            let _ = self.parse_aux_data(gb);
        }

        // Core extensions.
        if !self.ext_audio_present {
            return;
        }

        let buf = gb.buf();
        let size_in_bits = gb.size_in_bits();
        let word = |pos: usize| -> u32 {
            u32::from_be_bytes([buf[pos * 4], buf[pos * 4 + 1], buf[pos * 4 + 2], buf[pos * 4 + 3]])
        };

        let nwords = (self.frame_size / 4).min(size_in_bits / 32);
        if nwords == 0 {
            return;
        }
        let mut sync_pos = nwords - 1;
        let last_pos = gb.pos() / 32;
        let mut w2 = 0u32;

        // Search for extension sync words aligned on 4-byte boundary. The search must be done
        // backwards from the end of core frame to work around sync word aliasing issues.
        match self.ext_audio_type {
            EXT_AUDIO_XCH => {
                // The distance between XCH sync word and end of the core frame must be equal to
                // XCH frame size. Off by one error is allowed for compatibility with legacy
                // bitstreams. Minimum XCH frame size is 96 bytes. AMODE and PCHS are further
                // checked to reduce probability of alias sync detection.
                while sync_pos >= last_pos {
                    let w1 = word(sync_pos);
                    if w1 == SYNC_XCH {
                        let size = (w2 >> 22) as usize + 1;
                        let dist = self.frame_size as isize - sync_pos as isize * 4;
                        if size >= 96
                            && (size as isize == dist || size as isize - 1 == dist)
                            && (w2 >> 15 & 0x7f) == 0x08
                        {
                            self.xch_pos = sync_pos * 32 + 49;
                            break;
                        }
                    }
                    if sync_pos == 0 {
                        break;
                    }
                    sync_pos -= 1;
                    w2 = w1;
                }
            }
            EXT_AUDIO_X96 => {
                // The distance between X96 sync word and end of the core frame must be equal to
                // X96 frame size. Minimum X96 frame size is 96 bytes.
                while sync_pos >= last_pos {
                    let w1 = word(sync_pos);
                    if w1 == SYNC_X96 {
                        let size = (w2 >> 20) as usize + 1;
                        let dist = self.frame_size as isize - sync_pos as isize * 4;
                        if size >= 96 && size as isize == dist {
                            self.x96_pos = sync_pos * 32 + 44;
                            break;
                        }
                    }
                    if sync_pos == 0 {
                        break;
                    }
                    sync_pos -= 1;
                    w2 = w1;
                }
            }
            EXT_AUDIO_XXCH => {
                // XXCH frame header CRC must be valid. Minimum XXCH frame header size is 11
                // bytes.
                while sync_pos >= last_pos {
                    let w1 = word(sync_pos);
                    if w1 == SYNC_XXCH {
                        let size = (w2 >> 26) as usize + 1;
                        let dist = size_in_bits / 8 - sync_pos * 4;
                        if size >= 11
                            && size <= dist
                            && crc16(
                                0xffff,
                                &buf[(sync_pos + 1) * 4..(sync_pos + 1) * 4 + size - 4],
                            ) == 0
                        {
                            self.xxch_pos = sync_pos * 32;
                            break;
                        }
                    }
                    if sync_pos == 0 {
                        break;
                    }
                    sync_pos -= 1;
                    w2 = w1;
                }
            }
            _ => (),
        }
    }

    /// Parse a core frame (`ff_dca_core_parse`). `data` starts with the core sync word and may
    /// be followed by other substreams.
    pub fn parse(&mut self, data: &[u8]) -> Result<()> {
        self.ext_audio_mask = 0;
        self.xch_pos = 0;
        self.xxch_pos = 0;
        self.x96_pos = 0;

        let mut gb = BitReader::new(data);

        self.parse_frame_header(&mut gb)?;

        if !self.predictor_history {
            self.erase_adpcm_history();
        }

        self.parse_frame_data(&mut gb, HeaderType::Core, 0)?;
        self.parse_optional_info(&mut gb);

        // Workaround for DTS in WAV.
        if self.frame_size > data.len() {
            self.frame_size = data.len();
        }

        Ok(())
    }

    /// Parse core extensions (`ff_dca_core_parse_exss`).
    ///
    /// `core_data` is the data passed to [`CoreDecoder::parse`]. `exss` is the EXSS substream data
    /// and the asset descriptor, when present. If `skip_x96` is set, X96 is not decoded.
    pub fn parse_extensions(
        &mut self,
        core_data: &[u8],
        exss: Option<(&[u8], &ExssAsset)>,
        skip_x96: bool,
    ) -> Result<()> {
        // The EXSS asset, if it carries the given extension.
        let with_ext = |mask: u32| exss.filter(|(_, a)| a.extension_mask & mask != 0);

        // Parse (X)XCH.
        let mut ret = Ok(());
        let mut ext = 0;

        if let Some((data, asset)) = with_ext(EXSS_XXCH) {
            ret = match data.get(asset.xxch_offset..asset.xxch_offset + asset.xxch_size) {
                Some(d) => self.parse_xxch_frame(&mut BitReader::new(d)),
                None => decode_error("dca: invalid xxch asset range"),
            };
            ext = EXSS_XXCH;
        }
        else if self.xxch_pos != 0 {
            let mut gb = BitReader::new(core_data);
            gb.set_pos(self.xxch_pos);
            ret = self.parse_xxch_frame(&mut gb);
            ext = CSS_XXCH;
        }
        else if self.xch_pos != 0 {
            let mut gb = BitReader::new(core_data);
            gb.set_pos(self.xch_pos);
            ret = self.parse_xch_frame(&mut gb);
            ext = CSS_XCH;
        }

        if ret.is_err() {
            // Revert to primary channel set in case (X)XCH parsing fails.
            self.nchannels = usize::from(CHANNELS_PER_AMODE[self.audio_mode]);
            self.ch_mask = AUDIO_MODE_CH_MASK[self.audio_mode];
            if self.lfe_present != 0 {
                self.ch_mask |= MASK_LFE1;
            }
        }
        else {
            self.ext_audio_mask |= ext;
        }

        // Parse XBR.
        if let Some((data, asset)) = with_ext(EXSS_XBR) {
            let range = data.get(asset.xbr_offset..asset.xbr_offset + asset.xbr_size);
            if let Some(d) = range {
                if self.parse_xbr_frame(&mut BitReader::new(d)).is_ok() {
                    self.ext_audio_mask |= EXSS_XBR;
                }
            }
        }

        // Parse X96 unless decoding XLL.
        if !skip_x96 {
            if let Some((data, asset)) = with_ext(EXSS_X96) {
                let range = data.get(asset.x96_offset..asset.x96_offset + asset.x96_size);
                if let Some(d) = range {
                    if self.parse_x96_frame_exss(&mut BitReader::new(d)).is_ok() {
                        self.ext_audio_mask |= EXSS_X96;
                    }
                }
            }
            else if self.x96_pos != 0 {
                let mut gb = BitReader::new(core_data);
                gb.set_pos(self.x96_pos);
                if self.parse_x96_frame(&mut gb).is_ok() {
                    self.ext_audio_mask |= CSS_X96;
                }
            }
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Synthesis
    // ---------------------------------------------------------------------------------------

    fn map_prm_ch_to_spkr(&self, ch: usize) -> Option<usize> {
        // Try to map this channel to core first.
        let mut pos = usize::from(CHANNELS_PER_AMODE[self.audio_mode]);
        if ch < pos {
            let spkr = PRM_CH_TO_SPKR_MAP[self.audio_mode][ch] as usize;
            if self.ext_audio_mask & (CSS_XXCH | EXSS_XXCH) != 0 {
                if self.xxch_core_mask & (1 << spkr) != 0 {
                    return Some(spkr);
                }
                if spkr == spkr::LS && self.xxch_core_mask & MASK_LSS != 0 {
                    return Some(spkr::LSS);
                }
                if spkr == spkr::RS && self.xxch_core_mask & MASK_RSS != 0 {
                    return Some(spkr::RSS);
                }
                return None;
            }
            return Some(spkr);
        }

        // Then XCH.
        if self.ext_audio_mask & CSS_XCH != 0 && ch == pos {
            return Some(spkr::CS);
        }

        // Then XXCH.
        if self.ext_audio_mask & (CSS_XXCH | EXSS_XXCH) != 0 {
            for spkr in spkr::CS..self.xxch_mask_nbits {
                if self.xxch_spkr_mask & (1 << spkr) != 0 {
                    if pos == ch {
                        return Some(spkr);
                    }
                    pos += 1;
                }
            }
        }

        // No mapping.
        None
    }

    /// Map a speaker to the speaker that carries its signal in the output: the speaker itself if
    /// it is in the channel mask, `Lss`/`Rss` fall back to `Ls`/`Rs`.
    pub fn map_spkr(&self, spkr: usize) -> Option<usize> {
        if spkr < 32 && self.ch_mask & (1 << spkr) != 0 {
            return Some(spkr);
        }
        if spkr == spkr::LSS && self.ch_mask & MASK_LS != 0 {
            return Some(spkr::LS);
        }
        if spkr == spkr::RSS && self.ch_mask & MASK_RS != 0 {
            return Some(spkr::RS);
        }
        None
    }

    /// Bit-exact fixed-point synthesis of the subband samples of the frame that was just parsed
    /// into [`CoreDecoder::out_fixed`], the primary channel set only (no extension downmix
    /// undoing). Port of `ff_dca_core_filter_fixed`.
    ///
    /// `x96_synth` is `0` to use 64-band synthesis only if X96 data was decoded, `1` to force
    /// 64-band synthesis while ignoring X96 data and `-1` to force 32-band synthesis. Sets
    /// [`CoreDecoder::output_rate`] and [`CoreDecoder::npcmsamples`].
    pub fn filter_fixed(&mut self, x96_synth: i32) -> Result<()> {
        let mut x96_synth = x96_synth;
        let mut x96_nchannels = 0;

        // Externally set x96_synth flag implies that X96 synthesis should be enabled, yet actual
        // X96 subband data should be discarded. This is a special case for the lossless residual
        // decoder that ignores X96 data if present.
        if x96_synth == 0 && self.has_x96() {
            x96_nchannels = self.x96_nchannels;
            x96_synth = 1;
        }
        let x96 = x96_synth > 0;

        let nsamples = (self.npcmblocks * PCMBLOCK_SAMPLES) << usize::from(x96);
        self.output_rate = self.sample_rate << u32::from(x96);
        self.npcmsamples = nsamples;

        // Grow the PCM output buffers.
        for spkr in 0..spkr::COUNT {
            if self.ch_mask & (1 << spkr) != 0 && self.out_fixed[spkr].len() < nsamples {
                self.out_fixed[spkr].resize(nsamples, 0);
            }
        }

        // Handle change of filtering mode.
        self.set_filter_mode(u8::from(x96) | FILTER_MODE_FIXED);

        // Filter primary channels.
        for ch in 0..self.nchannels {
            // Map this primary channel to speaker.
            let Some(spkr) = self.map_prm_ch_to_spkr(ch)
            else {
                return decode_error("dca: unmapped channel");
            };
            if self.out_fixed[spkr].len() < nsamples {
                return decode_error("dca: output speaker not in channel mask");
            }

            let lo = Subbands::new(&self.subband_samples, ch * SUBBANDS * SUBBAND_STRIDE);
            let pcm = &mut self.out_fixed[spkr][..nsamples];

            if x96 {
                let hi = if ch < x96_nchannels {
                    Some(Subbands::new(
                        &self.x96_subband_samples,
                        ch * SUBBANDS_X96 * SUBBAND_STRIDE,
                    ))
                }
                else {
                    None
                };
                sub_qmf64_fixed(&mut self.synth_fixed[ch], pcm, &lo, hi.as_ref(), self.npcmblocks);
            }
            else {
                sub_qmf32_fixed(
                    &mut self.synth_fixed[ch],
                    self.filter_perfect,
                    pcm,
                    &lo,
                    self.npcmblocks,
                );
            }
        }

        // Filter LFE channel.
        if self.lfe_present != 0 {
            let nlfesamples = self.npcmblocks >> 1;

            // Check LFF.
            if self.lfe_present == 1 {
                return decode_error("dca: fixed point mode doesn't support LFF=1");
            }
            if self.out_fixed[spkr::LFE1].len() < nsamples {
                return decode_error("dca: LFE not in channel mask");
            }

            if x96 {
                // Interpolate into an intermediate buffer, then filter the 96 kHz oversampled
                // LFE PCM to attenuate high frequency (47.6 - 48.0 kHz) components of the
                // interpolation image.
                if self.lfe_tmp_fixed.len() < nsamples / 2 {
                    self.lfe_tmp_fixed.resize(nsamples / 2, 0);
                }
                let tmp = &mut self.lfe_tmp_fixed[..nsamples / 2];
                lfe_fir_fixed(tmp, &self.lfe_samples, LFE_HISTORY, self.npcmblocks);
                lfe_x96_fixed(
                    &mut self.out_fixed[spkr::LFE1][..nsamples],
                    tmp,
                    &mut self.lfe_x96_history_fixed,
                );
            }
            else {
                lfe_fir_fixed(
                    &mut self.out_fixed[spkr::LFE1][..nsamples],
                    &self.lfe_samples,
                    LFE_HISTORY,
                    self.npcmblocks,
                );
            }

            // Update LFE history.
            for n in (0..LFE_HISTORY).rev() {
                self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
            }
        }

        Ok(())
    }

    /// Check that the fixed-point output of `spkr` is valid for `n` samples.
    fn check_fixed(&self, spkr: usize, n: usize) -> Result<()> {
        if spkr >= spkr::COUNT || self.ch_mask & (1 << spkr) == 0 || self.out_fixed[spkr].len() < n
        {
            return decode_error("dca: output speaker not in channel mask");
        }
        Ok(())
    }

    /// Mutable access to the fixed-point output of two distinct, valid speakers.
    fn fixed_pair(&mut self, a: usize, b: usize, n: usize) -> Result<(&mut [i32], &mut [i32])> {
        self.check_fixed(a, n)?;
        self.check_fixed(b, n)?;
        if a == b {
            return decode_error("dca: duplicate output speaker");
        }
        let v = &mut self.out_fixed;
        if a < b {
            let (l, r) = v.split_at_mut(b);
            Ok((&mut l[a][..n], &mut r[0][..n]))
        }
        else {
            let (l, r) = v.split_at_mut(a);
            Ok((&mut r[0][..n], &mut l[b][..n]))
        }
    }

    /// Fixed-point equivalent of [`CoreDecoder::filter_frame`]: synthesize the frame (unless
    /// `already_filtered`, as when the lossless extension filtered it), undo the embedded XCH and
    /// XXCH downmix and the sum/difference coding, and leave 24-bit PCM in
    /// [`CoreDecoder::out_fixed`]. Port of upstream `filter_frame_fixed` without stereo downmix.
    pub fn filter_frame_fixed(&mut self, already_filtered: bool) -> Result<FrameInfo> {
        if !already_filtered {
            self.filter_fixed(0)?;
        }

        let nsamples = self.npcmsamples;

        // Undo embedded XCH downmix.
        if self.es_format && self.ext_audio_mask & CSS_XCH != 0 && self.audio_mode >= AMODE_2F2R {
            self.check_fixed(spkr::CS, nsamples)?;
            let cs = std::mem::take(&mut self.out_fixed[spkr::CS]);
            let res = self.fixed_pair(spkr::LS, spkr::RS, nsamples);
            let res = res.map(|(ls, rs)| dmix_sub_xch(ls, rs, &cs[..nsamples]));
            self.out_fixed[spkr::CS] = cs;
            res?;
        }

        // Undo embedded XXCH downmix.
        if self.ext_audio_mask & (CSS_XXCH | EXSS_XXCH) != 0 && self.xxch_dmix_embedded {
            let scale_inv = self.xxch_dmix_scale_inv;
            let mut coeff_ptr = 0;
            let xch_base = usize::from(CHANNELS_PER_AMODE[self.audio_mode]);
            let nbits = self.xxch_mask_nbits.min(spkr::COUNT);

            // Undo embedded core downmix pre-scaling.
            for spkr in 0..nbits {
                if self.xxch_core_mask & (1 << spkr) != 0 {
                    self.check_fixed(spkr, nsamples)?;
                    dmix_scale_inv(&mut self.out_fixed[spkr][..nsamples], scale_inv);
                }
            }

            // Undo downmix.
            for ch in xch_base..self.nchannels {
                let Some(src_spkr) = self.map_prm_ch_to_spkr(ch)
                else {
                    return decode_error("dca: unmapped channel");
                };
                let Some(&dmix_mask) = self.xxch_dmix_mask.get(ch - xch_base)
                else {
                    return decode_error("dca: invalid XXCH downmix channel");
                };
                for spkr in 0..nbits {
                    if dmix_mask & (1 << spkr) != 0 {
                        let Some(&c) = self.xxch_dmix_coeff.get(coeff_ptr)
                        else {
                            return decode_error("dca: invalid XXCH downmix coefficients");
                        };
                        coeff_ptr += 1;
                        let coeff = mul16(c, scale_inv);
                        if coeff != 0 {
                            if spkr == src_spkr {
                                self.check_fixed(spkr, nsamples)?;
                                for v in &mut self.out_fixed[spkr][..nsamples] {
                                    *v = v.wrapping_sub(crate::dsp::mul15(*v, coeff));
                                }
                            }
                            else {
                                let (dst, src) = self.fixed_pair(spkr, src_spkr, nsamples)?;
                                dmix_sub(dst, src, coeff);
                            }
                        }
                    }
                }
            }
        }

        if self.ext_audio_mask & (CSS_XXCH | CSS_XCH | EXSS_XXCH) == 0 {
            // Front sum/difference decoding.
            if (self.sumdiff_front && self.audio_mode > AMODE_MONO)
                || self.audio_mode == AMODE_STEREO_SUMDIFF
            {
                let (l, r) = self.fixed_pair(spkr::L, spkr::R, nsamples)?;
                butterflies_fixed(l, r);
            }

            // Surround sum/difference decoding.
            if self.sumdiff_surround && self.audio_mode >= AMODE_2F2R {
                let (ls, rs) = self.fixed_pair(spkr::LS, spkr::RS, nsamples)?;
                butterflies_fixed(ls, rs);
            }
        }

        // Final 24-bit clipping of the output.
        for spkr in 0..spkr::COUNT {
            if self.ch_mask & (1 << spkr) != 0 {
                for v in &mut self.out_fixed[spkr][..nsamples] {
                    *v = clip23(*v);
                }
            }
        }

        Ok(FrameInfo { nsamples, sample_rate: self.output_rate, ch_mask: self.ch_mask })
    }

    /// Synthesize PCM from the subband samples of the frame that was just parsed into
    /// [`CoreDecoder::out`].
    pub fn filter_frame(&mut self) -> Result<FrameInfo> {
        let x96_synth = self.has_x96();
        let x96_nchannels = if x96_synth { self.x96_nchannels } else { 0 };

        let nsamples = (self.npcmblocks * PCMBLOCK_SAMPLES) << usize::from(x96_synth);

        for spkr in 0..spkr::COUNT {
            if self.ch_mask & (1 << spkr) != 0 && self.out[spkr].len() != nsamples {
                self.out[spkr].resize(nsamples, 0.0);
            }
        }

        // Handle change of filtering mode.
        self.set_filter_mode(u8::from(x96_synth));

        let scale = 1.0 / (1u32 << (17 - u32::from(x96_synth))) as f32;

        // Filter primary channels.
        for ch in 0..self.nchannels {
            // Map this primary channel to speaker.
            let Some(spkr) = self.map_prm_ch_to_spkr(ch)
            else {
                return decode_error("dca: unmapped channel");
            };

            let lo = Subbands::new(&self.subband_samples, ch * SUBBANDS * SUBBAND_STRIDE);
            let pcm = &mut self.out[spkr][..nsamples];

            if x96_synth {
                let hi = if ch < x96_nchannels {
                    Some(Subbands::new(
                        &self.x96_subband_samples,
                        ch * SUBBANDS_X96 * SUBBAND_STRIDE,
                    ))
                }
                else {
                    None
                };
                self.filters.sub_qmf64(
                    &mut self.synth[ch],
                    pcm,
                    &lo,
                    hi.as_ref(),
                    self.npcmblocks,
                    scale,
                );
            }
            else {
                self.filters.sub_qmf32(
                    &mut self.synth[ch],
                    self.filter_perfect,
                    pcm,
                    &lo,
                    self.npcmblocks,
                    scale,
                );
            }
        }

        // Filter LFE channel.
        if self.lfe_present != 0 {
            let dec_select = usize::from(self.lfe_present == 1);
            let nlfesamples = self.npcmblocks >> (dec_select + 1);

            if x96_synth {
                // Interpolate into an intermediate buffer, then filter the 96 kHz oversampled
                // LFE PCM to attenuate high frequency (47.6 - 48.0 kHz) components of the
                // interpolation image.
                self.lfe_tmp.resize(nsamples / 2, 0.0);
                lfe_fir(
                    &mut self.lfe_tmp,
                    &self.lfe_samples,
                    LFE_HISTORY,
                    self.npcmblocks,
                    dec_select,
                );
                lfe_x96(
                    &mut self.out[spkr::LFE1][..nsamples],
                    &self.lfe_tmp,
                    &mut self.lfe_x96_history,
                );
            }
            else {
                lfe_fir(
                    &mut self.out[spkr::LFE1][..nsamples],
                    &self.lfe_samples,
                    LFE_HISTORY,
                    self.npcmblocks,
                    dec_select,
                );
            }

            // Update LFE history.
            for n in (0..LFE_HISTORY).rev() {
                self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
            }
        }

        // Undo embedded XCH downmix.
        if self.es_format && self.ext_audio_mask & CSS_XCH != 0 && self.audio_mode >= AMODE_2F2R {
            let k = -(std::f64::consts::FRAC_1_SQRT_2 as f32);
            self.fmac(spkr::LS, spkr::CS, k, nsamples);
            self.fmac(spkr::RS, spkr::CS, k, nsamples);
        }

        // Undo embedded XXCH downmix.
        if self.ext_audio_mask & (CSS_XXCH | EXSS_XXCH) != 0 && self.xxch_dmix_embedded {
            let scale_inv = self.xxch_dmix_scale_inv as f32 * (1.0 / 65536.0);
            let mut coeff_ptr = 0;
            let xch_base = usize::from(CHANNELS_PER_AMODE[self.audio_mode]);

            // Undo downmix.
            for ch in xch_base..self.nchannels {
                let Some(src_spkr) = self.map_prm_ch_to_spkr(ch)
                else {
                    return decode_error("dca: unmapped channel");
                };
                for spkr in 0..self.xxch_mask_nbits {
                    if self.xxch_dmix_mask[ch - xch_base] & (1 << spkr) != 0 {
                        let coeff = self.xxch_dmix_coeff[coeff_ptr];
                        coeff_ptr += 1;
                        if coeff != 0 {
                            self.fmac(spkr, src_spkr, coeff as f32 * (-1.0 / 32768.0), nsamples);
                        }
                    }
                }
            }

            // Undo embedded core downmix pre-scaling.
            for spkr in 0..self.xxch_mask_nbits {
                if self.xxch_core_mask & (1 << spkr) != 0 {
                    for v in &mut self.out[spkr][..nsamples] {
                        *v *= scale_inv;
                    }
                }
            }
        }

        if self.ext_audio_mask & (CSS_XXCH | CSS_XCH | EXSS_XXCH) == 0 {
            // Front sum/difference decoding.
            if (self.sumdiff_front && self.audio_mode > AMODE_MONO)
                || self.audio_mode == AMODE_STEREO_SUMDIFF
            {
                self.butterflies(spkr::L, spkr::R, nsamples);
            }

            // Surround sum/difference decoding.
            if self.sumdiff_surround && self.audio_mode >= AMODE_2F2R {
                self.butterflies(spkr::LS, spkr::RS, nsamples);
            }
        }

        Ok(FrameInfo {
            nsamples,
            sample_rate: self.sample_rate << usize::from(x96_synth),
            ch_mask: self.ch_mask,
        })
    }

    /// `out[dst] += out[src] * k`.
    fn fmac(&mut self, dst: usize, src: usize, k: f32, nsamples: usize) {
        if dst == src {
            for v in &mut self.out[dst][..nsamples] {
                *v += *v * k;
            }
            return;
        }
        let s = std::mem::take(&mut self.out[src]);
        for (d, &x) in self.out[dst][..nsamples].iter_mut().zip(&s[..nsamples]) {
            *d += x * k;
        }
        self.out[src] = s;
    }

    /// `(a, b) = (a + b, a - b)`.
    fn butterflies(&mut self, a: usize, b: usize, nsamples: usize) {
        let mut vb = std::mem::take(&mut self.out[b]);
        for (x, y) in self.out[a][..nsamples].iter_mut().zip(&mut vb[..nsamples]) {
            let t = *x - *y;
            *x += *y;
            *y = t;
        }
        self.out[b] = vb;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = include_bytes!("../tests/data/sine_5_1.dts");
    const FRAME_SIZE: usize = 1884;

    /// The fixed-point and floating-point core decoders are independent implementations of the
    /// same filter bank and must agree to within the precision of the fixed-point path.
    #[test]
    fn fixed_core_matches_float_core() {
        let mut float = CoreDecoder::new();
        let mut fixed = CoreDecoder::new();

        for frame in STREAM.chunks(FRAME_SIZE) {
            float.parse(frame).unwrap();
            float.parse_extensions(frame, None, false).unwrap();
            let info = float.filter_frame().unwrap();

            fixed.parse(frame).unwrap();
            fixed.parse_extensions(frame, None, false).unwrap();
            let info_fixed = fixed.filter_frame_fixed(false).unwrap();

            assert_eq!(info.nsamples, info_fixed.nsamples);
            assert_eq!(info.sample_rate, info_fixed.sample_rate);
            assert_eq!(info.ch_mask, info_fixed.ch_mask);

            for spkr in 0..spkr::COUNT {
                // The reference fixed-point LFE interpolation differs from the floating-point one
                // by design (it is not a rounding difference), only compare the full range
                // channels.
                if info.ch_mask & (1 << spkr) == 0 || spkr == spkr::LFE1 {
                    continue;
                }
                for n in 0..info.nsamples {
                    let a = float.out[spkr][n];
                    let b = fixed.out_fixed[spkr][n] as f32 / (1 << 23) as f32;
                    assert!((a - b).abs() < 1e-4, "speaker {spkr} sample {n}: {a} vs {b}");
                }
            }
        }
    }
}
