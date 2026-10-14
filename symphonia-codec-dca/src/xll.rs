// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dca_xll.c (LGPL-2.1+), see NOTICE.

//! DTS-HD Master Audio (XLL) lossless extension decoder.
//!
//! XLL carries, for each channel set, the (optionally lossily predicted) residual between the
//! original PCM and the output of the fixed-point core decoder. Reconstruction is bit exact: the
//! fixed-point core output (see [`CoreDecoder::filter_fixed`]) is combined with the decoded
//! residual of each channel.

use crate::bits::BitReader;
use crate::decoder::{CoreDecoder, clip23, norm, spkr};
use crate::dsp::{mul15, mul16};
use crate::exss::{ExssAsset, SAMPLING_FREQS};
use crate::tables::{DMIX_PRIMARY_NCH, DMIXTABLE, INV_DMIXTABLE, XLL_BAND_COEFF, XLL_REFL_COEFF};

const SYNC_XLL: u32 = 0x41a2_9547;

const CHSETS_MAX: usize = 3;
const CHANNELS_MAX: usize = 8;
const BANDS_MAX: usize = 2;
const ADAPT_PRED_ORDER_MAX: usize = 16;
const DECI_HISTORY_MAX: usize = 8;
const DMIX_SCALES_MAX: usize = (CHSETS_MAX - 1) * CHANNELS_MAX;
const DMIX_COEFFS_MAX: usize = DMIX_SCALES_MAX * CHANNELS_MAX;
const PBR_BUFFER_MAX: usize = 240 << 10;

const DMIXTABLE_SIZE: u32 = 242;
const INV_DMIXTABLE_SIZE: u32 = 201;
const DMIXTABLE_OFFSET: u32 = DMIXTABLE_SIZE - INV_DMIXTABLE_SIZE;

const DMIX_TYPE_COUNT: u32 = 7;

/// Errors of the XLL decoder.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum XllError {
    /// The XLL data did not start on a synchronisation word. This is expected when jumping into
    /// the middle of a peak bit rate smoothing period.
    Again,
    /// The XLL data is invalid, unsupported, or inconsistent with the core. The caller should fall
    /// back to the core, if there is one.
    Invalid,
    /// The XLL data can't be decoded and the core can't replace it.
    Hard,
}

type XResult<T> = Result<T, XllError>;

// -------------------------------------------------------------------------------------------
// Entropy code readers
// -------------------------------------------------------------------------------------------

fn get_linear(gb: &mut BitReader<'_>, n: u32) -> i32 {
    let v = gb.get(n);
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn get_unary(gb: &mut BitReader<'_>, len: i64) -> u32 {
    let mut i = 0u32;
    while i64::from(i) < len && !gb.get1() {
        i += 1;
    }
    i
}

fn get_rice(gb: &mut BitReader<'_>, k: u32) -> i32 {
    let len = gb.left();
    let v = (get_unary(gb, len) << k) | gb.get(k);
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn get_array(gb: &mut BitReader<'_>, array: &mut [i32], n: u32) {
    for a in array {
        *a = gb.get(n) as i32;
    }
}

fn get_linear_array(gb: &mut BitReader<'_>, array: &mut [i32], n: u32) {
    if n == 0 {
        array.fill(0);
    }
    else {
        for a in array {
            *a = get_linear(gb, n);
        }
    }
}

fn get_rice_array(gb: &mut BitReader<'_>, array: &mut [i32], k: u32) -> XResult<()> {
    for a in array {
        if gb.left() <= i64::from(k) {
            return Err(XllError::Invalid);
        }
        *a = get_rice(gb, k);
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Decoder state
// -------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Band {
    /// Pairwise channel decorrelation flag.
    decor_enabled: bool,
    /// Original channel order.
    orig_order: [usize; CHANNELS_MAX],
    /// Pairwise channel coefficients.
    decor_coeff: [i32; CHANNELS_MAX / 2],

    /// Adaptive predictor order.
    adapt_pred_order: [usize; CHANNELS_MAX],
    /// Highest adaptive predictor order.
    highest_pred_order: usize,
    /// Fixed predictor order.
    fixed_pred_order: [usize; CHANNELS_MAX],
    /// Adaptive predictor reflection coefficients.
    adapt_refl_coeff: [[i32; ADAPT_PRED_ORDER_MAX]; CHANNELS_MAX],

    /// Downmix performed by encoder in frequency band.
    dmix_embedded: bool,

    /// Size of LSB section in any segment.
    lsb_section_size: usize,
    /// Number of bits to represent the samples in LSB part.
    nscalablelsbs: [u32; CHANNELS_MAX],
    /// Number of bits discarded by authoring.
    bit_width_adjust: [u32; CHANNELS_MAX],

    /// Start of each channel's MSB samples in the channel set's MSB buffer.
    msb_off: [usize; CHANNELS_MAX],
    /// Start of each channel's LSB samples in the channel set's LSB buffer.
    lsb_off: [Option<usize>; CHANNELS_MAX],
}

#[derive(Clone)]
struct ChSet {
    // Channel set header.
    /// Number of channels in the channel set.
    nchannels: usize,
    /// Residual encoding mask (0 - residual, 1 - full channel).
    residual_encode: u32,
    /// PCM bit resolution.
    pcm_bit_res: u32,
    /// Storage bit resolution (16, 20 or 24).
    storage_bit_res: u32,
    /// Original sampling frequency (of the first frequency band).
    freq: u32,

    /// Primary channel set flag.
    primary_chset: bool,
    /// Downmix coefficients present in stream.
    dmix_coeffs_present: bool,
    /// Downmix already performed by encoder.
    dmix_embedded: bool,
    /// Primary channel set downmix type.
    dmix_type: u32,
    /// Whether the channel set is part of a hierarchy.
    hier_chset: bool,
    /// Number of preceding channels in a hierarchy.
    hier_ofs: usize,
    /// Downmixing coefficients.
    dmix_coeff: [i32; DMIX_COEFFS_MAX],
    /// Downmixing scales.
    dmix_scale: [i32; DMIX_SCALES_MAX],
    /// Inverse downmixing scales.
    dmix_scale_inv: [i32; DMIX_SCALES_MAX],
    /// Channel mask for set.
    ch_mask: u32,
    /// Channel to speaker map.
    ch_remap: [usize; CHANNELS_MAX],

    /// Number of frequency bands (1 or 2).
    nfreqbands: usize,
    /// Number of bits to read bit allocation coding parameter.
    nabits: u32,

    /// Frequency bands.
    bands: [Band; BANDS_MAX],

    // Frequency band coding parameters.
    /// Segment type.
    seg_common: bool,
    /// Rice coding flag.
    rice_code_flag: [bool; CHANNELS_MAX],
    /// Binary code length for isolated samples.
    bitalloc_hybrid_linear: [u32; CHANNELS_MAX],
    /// Coding parameter for part A of segment.
    bitalloc_part_a: [u32; CHANNELS_MAX],
    /// Coding parameter for part B of segment.
    bitalloc_part_b: [u32; CHANNELS_MAX],
    /// Number of samples in part A of segment.
    nsamples_part_a: [usize; CHANNELS_MAX],

    /// Decimator history for frequency band 1.
    deci_history: [[i32; DECI_HISTORY_MAX]; CHANNELS_MAX],

    /// MSB sample buffer: for each band and channel `nframesamples` samples, preceded by
    /// `DECI_HISTORY_MAX` samples of padding if the channel set has two frequency bands.
    msb: Vec<i32>,
    /// LSB sample buffer.
    lsb: Vec<i32>,
    /// Frequency band assembly buffer.
    assembled: Vec<i32>,
}

impl Default for ChSet {
    fn default() -> Self {
        ChSet {
            nchannels: 0,
            residual_encode: 0,
            pcm_bit_res: 0,
            storage_bit_res: 0,
            freq: 0,
            primary_chset: false,
            dmix_coeffs_present: false,
            dmix_embedded: false,
            dmix_type: 0,
            hier_chset: false,
            hier_ofs: 0,
            dmix_coeff: [0; DMIX_COEFFS_MAX],
            dmix_scale: [0; DMIX_SCALES_MAX],
            dmix_scale_inv: [0; DMIX_SCALES_MAX],
            ch_mask: 0,
            ch_remap: [0; CHANNELS_MAX],
            nfreqbands: 0,
            nabits: 0,
            bands: Default::default(),
            seg_common: false,
            rice_code_flag: [false; CHANNELS_MAX],
            bitalloc_hybrid_linear: [0; CHANNELS_MAX],
            bitalloc_part_a: [0; CHANNELS_MAX],
            bitalloc_part_b: [0; CHANNELS_MAX],
            nsamples_part_a: [0; CHANNELS_MAX],
            deci_history: [[0; DECI_HISTORY_MAX]; CHANNELS_MAX],
            msb: Vec::new(),
            lsb: Vec::new(),
            assembled: Vec::new(),
        }
    }
}

impl ChSet {
    fn is_hier_dmix(&self) -> bool {
        !self.primary_chset && self.dmix_embedded && self.hier_chset
    }
}

/// The location of an output channel's samples.
#[derive(Copy, Clone)]
enum Loc {
    /// MSB sample buffer of a channel set at an offset.
    Msb(usize, usize),
    /// Frequency band assembly buffer of a channel set at an offset.
    Assembled(usize, usize),
}

/// Information on a decoded XLL frame, the samples are in [`XllDecoder::out`].
pub struct XllFrame {
    /// Number of PCM samples per channel.
    pub nsamples: usize,
    /// Sample rate.
    pub sample_rate: u32,
    /// Speaker mask (DTS speaker indices) of the channels.
    pub ch_mask: u32,
    /// Sample resolution of the output: samples span `-(1 << (bits - 1))..(1 << (bits - 1))`.
    pub bits: u32,
}

pub struct XllDecoder {
    /// Number of bytes in a lossless frame.
    frame_size: usize,
    /// Number of channel sets per frame.
    pub nchsets: usize,
    /// Number of segments per frame.
    nframesegs: usize,
    /// log2(nsegsamples).
    nsegsamples_log2: u32,
    /// Samples in segment per one frequency band.
    nsegsamples: usize,
    /// Samples in frame per one frequency band.
    nframesamples: usize,
    /// Number of bits used to read segment size.
    seg_size_nbits: u32,
    /// Presence of CRC16 within each frequency band.
    band_crc_present: u32,
    /// MSB/LSB split flag.
    scalable_lsbs: bool,
    /// Number of bits used to read channel mask.
    ch_mask_nbits: u32,
    /// Fixed LSB width.
    fixed_lsb_width: i32,

    chset: [ChSet; CHSETS_MAX],

    /// NAVI table.
    navi: Vec<usize>,

    /// Highest number of frequency bands.
    nfreqbands: usize,
    /// Total number of channels in a hierarchy.
    nchannels: usize,
    /// Number of channel sets that have residual encoded channels.
    pub nreschsets: usize,
    /// Number of active channel sets to decode.
    nactivechsets: usize,

    /// Previous DTS-HD stream ID for detecting changes.
    hd_stream_id: u32,

    /// Peak bit rate (PBR) smoothing buffer.
    pbr_buffer: Vec<u8>,
    /// Length in bytes of data currently buffered.
    pbr_length: usize,
    /// Delay in frames before decoding buffered data.
    pbr_delay: u32,

    output_loc: [Option<Loc>; spkr::COUNT],
    output_mask: u32,

    /// Output PCM indexed by DTS speaker, valid after a successful [`XllDecoder::filter_frame`].
    pub out: Vec<Vec<i32>>,
}

impl Default for XllDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl XllDecoder {
    pub fn new() -> Self {
        XllDecoder {
            frame_size: 0,
            nchsets: 0,
            nframesegs: 0,
            nsegsamples_log2: 0,
            nsegsamples: 0,
            nframesamples: 0,
            seg_size_nbits: 0,
            band_crc_present: 0,
            scalable_lsbs: false,
            ch_mask_nbits: 0,
            fixed_lsb_width: 0,
            chset: Default::default(),
            navi: Vec::new(),
            nfreqbands: 0,
            nchannels: 0,
            nreschsets: 0,
            nactivechsets: 0,
            hd_stream_id: 0,
            pbr_buffer: Vec::new(),
            pbr_length: 0,
            pbr_delay: 0,
            output_loc: [None; spkr::COUNT],
            output_mask: 0,
            out: vec![Vec::new(); spkr::COUNT],
        }
    }

    /// Sample rate of the first channel set (the rate of its first frequency band).
    pub fn chset0_freq(&self) -> u32 {
        self.chset[0].freq
    }

    /// Reset all state that persists between frames.
    pub fn flush(&mut self) {
        self.clear_pbr();
    }

    fn clear_pbr(&mut self) {
        self.pbr_length = 0;
        self.pbr_delay = 0;
    }

    // ---------------------------------------------------------------------------------------
    // Parsing
    // ---------------------------------------------------------------------------------------

    fn parse_dmix_coeffs(&mut self, gb: &mut BitReader<'_>, ci: usize) -> XResult<()> {
        let c = &mut self.chset[ci];

        // Size of downmix coefficient matrix.
        let m = if c.primary_chset {
            usize::from(DMIX_PRIMARY_NCH[c.dmix_type as usize])
        }
        else {
            c.hier_ofs
        };
        let mut coeff_ptr = 0;

        for i in 0..m {
            let mut scale_inv = 0i32;

            // Downmix scale (only for non-primary channel sets).
            if !c.primary_chset {
                let code = gb.get(9);
                let sign = (code >> 8) as i32 - 1;
                let index = (code & 0xff).wrapping_sub(DMIXTABLE_OFFSET);
                if index >= INV_DMIXTABLE_SIZE {
                    return Err(XllError::Invalid);
                }
                let scale = i32::from(DMIXTABLE[(index + DMIXTABLE_OFFSET) as usize]);
                scale_inv = INV_DMIXTABLE[index as usize] as i32;
                c.dmix_scale[i] = (scale ^ sign).wrapping_sub(sign);
                c.dmix_scale_inv[i] = (scale_inv ^ sign).wrapping_sub(sign);
            }

            // Downmix coefficients.
            for _ in 0..c.nchannels {
                let code = gb.get(9);
                let sign = (code >> 8) as i32 - 1;
                let index = code & 0xff;
                if index >= DMIXTABLE_SIZE {
                    return Err(XllError::Invalid);
                }
                let mut coeff = i32::from(DMIXTABLE[index as usize]);
                if !c.primary_chset {
                    // Multiply by |InvDmixScale| to get |UndoDmixScale|.
                    coeff = mul16(scale_inv, coeff);
                }
                if coeff_ptr >= DMIX_COEFFS_MAX {
                    return Err(XllError::Invalid);
                }
                c.dmix_coeff[coeff_ptr] = (coeff ^ sign).wrapping_sub(sign);
                coeff_ptr += 1;
            }
        }

        Ok(())
    }

    fn chs_parse_header(
        &mut self,
        gb: &mut BitReader<'_>,
        ci: usize,
        asset: &ExssAsset,
    ) -> XResult<()> {
        let header_pos = gb.pos();

        // Size of channel set sub-header.
        let header_size = gb.get(10) as usize + 1;

        // Number of channels in the channel set.
        let nchannels = gb.get(4) as usize + 1;
        if nchannels > CHANNELS_MAX {
            return Err(XllError::Invalid);
        }
        self.chset[ci].nchannels = nchannels;

        // Residual type.
        self.chset[ci].residual_encode = gb.get(nchannels as u32);

        // PCM bit resolution.
        let pcm_bit_res = gb.get(5) + 1;

        // Storage unit width.
        let storage_bit_res = gb.get(5) + 1;
        if storage_bit_res != 16 && storage_bit_res != 20 && storage_bit_res != 24 {
            return Err(XllError::Invalid);
        }

        if pcm_bit_res > storage_bit_res {
            return Err(XllError::Invalid);
        }
        self.chset[ci].pcm_bit_res = pcm_bit_res;
        self.chset[ci].storage_bit_res = storage_bit_res;

        // Original sampling frequency.
        let mut freq = SAMPLING_FREQS[gb.get(4) as usize];
        if freq > 192000 {
            return Err(XllError::Invalid);
        }

        // Sampling frequency modifier.
        if gb.get(2) != 0 {
            return Err(XllError::Invalid);
        }

        // Which replacement set this channel set is member of.
        if gb.get(2) != 0 {
            return Err(XllError::Invalid);
        }

        if asset.one_to_one_map_ch_to_spkr {
            // Primary channel set flag.
            let primary = gb.get1();
            if primary != (ci == 0) {
                return Err(XllError::Invalid);
            }
            self.chset[ci].primary_chset = primary;

            // Downmix coefficients present in stream.
            let dmix_coeffs_present = gb.get1();
            self.chset[ci].dmix_coeffs_present = dmix_coeffs_present;

            // Downmix already performed by encoder.
            self.chset[ci].dmix_embedded = dmix_coeffs_present && gb.get1();

            // Downmix type.
            if dmix_coeffs_present && primary {
                let dmix_type = gb.get(3);
                if dmix_type >= DMIX_TYPE_COUNT {
                    return Err(XllError::Invalid);
                }
                self.chset[ci].dmix_type = dmix_type;
            }

            // Whether the channel set is part of a hierarchy.
            self.chset[ci].hier_chset = gb.get1();
            if !self.chset[ci].hier_chset && self.nchsets != 1 {
                return Err(XllError::Invalid);
            }

            // Downmix coefficients.
            if dmix_coeffs_present {
                self.parse_dmix_coeffs(gb, ci)?;
            }

            // Channel mask enabled.
            if !gb.get1() {
                return Err(XllError::Invalid);
            }

            // Channel mask for set.
            let ch_mask = gb.get(self.ch_mask_nbits);
            if ch_mask.count_ones() as usize != nchannels {
                return Err(XllError::Invalid);
            }
            self.chset[ci].ch_mask = ch_mask;

            // Build the channel to speaker map.
            let mut j = 0;
            for i in 0..self.ch_mask_nbits as usize {
                if ch_mask & (1u32 << i) != 0 {
                    if i >= spkr::COUNT {
                        return Err(XllError::Invalid);
                    }
                    self.chset[ci].ch_remap[j] = i;
                    j += 1;
                }
            }
        }
        else {
            // Mapping coeffs present flag.
            if nchannels != 2 || self.nchsets != 1 || gb.get1() {
                return Err(XllError::Invalid);
            }

            // Setup for LtRt decoding.
            let c = &mut self.chset[ci];
            c.primary_chset = true;
            c.dmix_coeffs_present = false;
            c.dmix_embedded = false;
            c.hier_chset = false;
            c.ch_mask = (1 << spkr::L) | (1 << spkr::R);
            c.ch_remap[0] = spkr::L;
            c.ch_remap[1] = spkr::R;
        }

        let nfreqbands = if freq > 96000 {
            // Extra frequency bands flag.
            if gb.get1() {
                return Err(XllError::Invalid);
            }
            2
        }
        else {
            1
        };

        // Set the sampling frequency to that of the first frequency band. Frequency will be
        // doubled again after bands assembly.
        freq >>= nfreqbands - 1;
        self.chset[ci].nfreqbands = nfreqbands;
        self.chset[ci].freq = freq;

        // Verify that all channel sets have the same audio characteristics.
        if ci != 0 {
            let (p, c) = (&self.chset[0], &self.chset[ci]);
            if c.nfreqbands != p.nfreqbands
                || c.freq != p.freq
                || c.pcm_bit_res != p.pcm_bit_res
                || c.storage_bit_res != p.storage_bit_res
            {
                return Err(XllError::Invalid);
            }
        }

        // Determine number of bits to read bit allocation coding parameter.
        let mut nabits = if storage_bit_res > 16 {
            5
        }
        else if storage_bit_res > 8 {
            4
        }
        else {
            3
        };

        // Account for embedded downmix and decimator saturation.
        if (self.nchsets > 1 || nfreqbands > 1) && nabits < 5 {
            nabits += 1;
        }
        self.chset[ci].nabits = nabits;

        for band in 0..nfreqbands {
            let c = &mut self.chset[ci];
            let b = &mut c.bands[band];

            // Pairwise channel decorrelation.
            b.decor_enabled = gb.get1();
            if b.decor_enabled && nchannels > 1 {
                let ch_nbits = ceil_log2(nchannels as u32);

                // Original channel order.
                for i in 0..nchannels {
                    b.orig_order[i] = gb.get(ch_nbits) as usize;
                    if b.orig_order[i] >= nchannels {
                        return Err(XllError::Invalid);
                    }
                }

                // Pairwise channel coefficients.
                for i in 0..nchannels / 2 {
                    b.decor_coeff[i] = if gb.get1() { get_linear(gb, 7) } else { 0 };
                }
            }
            else {
                for i in 0..nchannels {
                    b.orig_order[i] = i;
                }
                for i in 0..nchannels / 2 {
                    b.decor_coeff[i] = 0;
                }
            }

            // Adaptive predictor order.
            b.highest_pred_order = 0;
            for i in 0..nchannels {
                b.adapt_pred_order[i] = gb.get(4) as usize;
                if b.adapt_pred_order[i] > b.highest_pred_order {
                    b.highest_pred_order = b.adapt_pred_order[i];
                }
            }
            if b.highest_pred_order > self.nsegsamples {
                return Err(XllError::Invalid);
            }

            // Fixed predictor order.
            for i in 0..nchannels {
                b.fixed_pred_order[i] =
                    if b.adapt_pred_order[i] != 0 { 0 } else { gb.get(2) as usize };
            }

            // Adaptive predictor quantized reflection coefficients.
            for i in 0..nchannels {
                for j in 0..b.adapt_pred_order[i] {
                    let k = get_linear(gb, 8);
                    if k == -128 {
                        return Err(XllError::Invalid);
                    }
                    b.adapt_refl_coeff[i][j] = if k < 0 {
                        -i32::from(XLL_REFL_COEFF[(-k) as usize])
                    }
                    else {
                        i32::from(XLL_REFL_COEFF[k as usize])
                    };
                }
            }

            // Downmix performed by encoder in extension frequency band.
            b.dmix_embedded = c.dmix_embedded && (band == 0 || gb.get1());

            // MSB/LSB split flag in extension frequency band.
            if (band == 0 && self.scalable_lsbs) || (band != 0 && gb.get1()) {
                // Size of LSB section in any segment.
                let size = gb.get(self.seg_size_nbits) as usize;
                if size > self.frame_size {
                    return Err(XllError::Invalid);
                }
                b.lsb_section_size = size;

                // Account for optional CRC bytes after LSB section.
                if b.lsb_section_size != 0
                    && (self.band_crc_present > 2 || (band == 0 && self.band_crc_present > 1))
                {
                    b.lsb_section_size += 2;
                }

                // Number of bits to represent the samples in LSB part.
                for i in 0..nchannels {
                    b.nscalablelsbs[i] = gb.get(4);
                    if b.nscalablelsbs[i] != 0 && b.lsb_section_size == 0 {
                        return Err(XllError::Invalid);
                    }
                }
            }
            else {
                b.lsb_section_size = 0;
                b.nscalablelsbs[..nchannels].fill(0);
            }

            // Scalable resolution flag in extension frequency band.
            if (band == 0 && self.scalable_lsbs) || (band != 0 && gb.get1()) {
                // Number of bits discarded by authoring.
                for i in 0..nchannels {
                    b.bit_width_adjust[i] = gb.get(4);
                }
            }
            else {
                b.bit_width_adjust[..nchannels].fill(0);
            }
        }

        // Reserved, byte align, CRC16 of channel set sub-header.
        if !gb.seek(header_pos + header_size * 8) {
            return Err(XllError::Invalid);
        }

        Ok(())
    }

    fn parse_common_header(&mut self, gb: &mut BitReader<'_>) -> XResult<()> {
        // XLL extension sync word.
        if gb.get(32) != SYNC_XLL {
            return Err(XllError::Again);
        }

        // Version number.
        let stream_ver = gb.get(4) + 1;
        if stream_ver > 1 {
            return Err(XllError::Invalid);
        }

        // Lossless frame header length.
        let header_size = gb.get(8) as usize + 1;

        // Number of bits used to read frame size.
        let frame_size_nbits = gb.get(5) + 1;

        // Number of bytes in a lossless frame.
        let frame_size = gb.get(frame_size_nbits) as usize;
        if frame_size >= PBR_BUFFER_MAX {
            return Err(XllError::Invalid);
        }
        self.frame_size = frame_size + 1;

        // Number of channels sets per frame.
        self.nchsets = gb.get(4) as usize + 1;
        if self.nchsets > CHSETS_MAX {
            return Err(XllError::Invalid);
        }

        // Number of segments per frame.
        let nframesegs_log2 = gb.get(4);
        self.nframesegs = 1 << nframesegs_log2;
        if self.nframesegs > 1024 {
            return Err(XllError::Invalid);
        }

        // Samples in segment per one frequency band for the first channel set. Maximum value is
        // 256 for sampling frequencies <= 48 kHz, 512 for sampling frequencies > 48 kHz.
        self.nsegsamples_log2 = gb.get(4);
        if self.nsegsamples_log2 == 0 {
            return Err(XllError::Invalid);
        }
        self.nsegsamples = 1 << self.nsegsamples_log2;
        if self.nsegsamples > 512 {
            return Err(XllError::Invalid);
        }

        // Samples in frame per one frequency band for the first channel set.
        let nframesamples_log2 = self.nsegsamples_log2 + nframesegs_log2;
        self.nframesamples = 1 << nframesamples_log2;
        if self.nframesamples > 65536 {
            return Err(XllError::Invalid);
        }

        // Number of bits used to read segment size.
        self.seg_size_nbits = gb.get(5) + 1;

        // Presence of CRC16 within each frequency band: 0 - none, 1 - at the end of MSB0, 2 - at
        // the end of MSB0 and LSB0, 3 - at the end of MSB0 and LSB0 and other frequency bands.
        self.band_crc_present = gb.get(2);

        // MSB/LSB split flag.
        self.scalable_lsbs = gb.get1();

        // Channel position mask.
        self.ch_mask_nbits = gb.get(5) + 1;

        // Fixed LSB width.
        self.fixed_lsb_width = if self.scalable_lsbs { gb.get(4) as i32 } else { 0 };

        // Reserved, byte align, header CRC16 protection.
        if !gb.seek(header_size * 8) {
            return Err(XllError::Invalid);
        }

        Ok(())
    }

    fn find_next_hier_dmix_chset(&self, ci: usize) -> Option<usize> {
        if self.chset[ci].hier_chset {
            ((ci + 1)..self.nchsets).find(|&i| self.chset[i].is_hier_dmix())
        }
        else {
            None
        }
    }

    fn prescale_down_mix(&mut self, ci: usize, oi: usize) {
        let (left, right) = self.chset.split_at_mut(oi);
        let (c, o) = (&mut left[ci], &right[0]);

        let mut coeff_ptr = 0;
        for i in 0..c.hier_ofs {
            let scale = o.dmix_scale[i];
            let scale_inv = o.dmix_scale_inv[i];
            c.dmix_scale[i] = mul15(c.dmix_scale[i], scale);
            c.dmix_scale_inv[i] = mul16(c.dmix_scale_inv[i], scale_inv);
            for j in 0..c.nchannels {
                let coeff = mul16(c.dmix_coeff[coeff_ptr], scale_inv);
                c.dmix_coeff[coeff_ptr] = mul15(coeff, o.dmix_scale[c.hier_ofs + j]);
                coeff_ptr += 1;
            }
        }
    }

    fn parse_sub_headers(&mut self, gb: &mut BitReader<'_>, asset: &ExssAsset) -> XResult<()> {
        // Parse channel set headers.
        self.nfreqbands = 0;
        self.nchannels = 0;
        self.nreschsets = 0;
        for i in 0..self.nchsets {
            self.chset[i].hier_ofs = self.nchannels;
            self.chs_parse_header(gb, i, asset)?;
            let c = &self.chset[i];
            if c.nfreqbands > self.nfreqbands {
                self.nfreqbands = c.nfreqbands;
            }
            if c.hier_chset {
                self.nchannels += c.nchannels;
            }
            if c.residual_encode != (1u32 << c.nchannels) - 1 {
                self.nreschsets += 1;
            }
        }

        // Pre-scale downmixing coefficients for all non-primary channel sets.
        for i in (1..self.nchsets).rev() {
            if self.chset[i].is_hier_dmix() {
                if let Some(o) = self.find_next_hier_dmix_chset(i) {
                    self.prescale_down_mix(i, o);
                }
            }
        }

        // All channel sets are decoded.
        self.nactivechsets = self.nchsets;

        Ok(())
    }

    fn parse_navi_table(&mut self, gb: &mut BitReader<'_>) -> XResult<()> {
        // Determine size of NAVI table.
        let navi_nb = self.nfreqbands * self.nframesegs * self.nchsets;
        if navi_nb > 1024 {
            return Err(XllError::Invalid);
        }

        self.navi.clear();
        for band in 0..self.nfreqbands {
            for _seg in 0..self.nframesegs {
                for c in &self.chset[..self.nchsets] {
                    let mut size = 0;
                    if c.nfreqbands > band {
                        size = gb.get(self.seg_size_nbits) as usize;
                        if size >= self.frame_size {
                            return Err(XllError::Invalid);
                        }
                        size += 1;
                    }
                    self.navi.push(size);
                }
            }
        }

        // Byte align, CRC16.
        gb.skip(gb.pos().wrapping_neg() & 7);
        gb.skip(16);

        Ok(())
    }

    fn chs_alloc_msb_band_data(&mut self, ci: usize) {
        let c = &mut self.chset[ci];
        let ndecisamples = if c.nfreqbands > 1 { DECI_HISTORY_MAX } else { 0 };
        let nchsamples = self.nframesamples + ndecisamples;
        let nsamples = nchsamples * c.nchannels * c.nfreqbands;

        if c.msb.len() < nsamples {
            c.msb.resize(nsamples, 0);
        }

        let mut off = ndecisamples;
        for band in 0..c.nfreqbands {
            for ch in 0..c.nchannels {
                c.bands[band].msb_off[ch] = off;
                off += nchsamples;
            }
        }
    }

    fn chs_alloc_lsb_band_data(&mut self, ci: usize) {
        let c = &mut self.chset[ci];

        // Determine number of frequency bands that have MSB/LSB split.
        let mut nsamples = 0;
        for band in &c.bands[..c.nfreqbands] {
            if band.lsb_section_size != 0 {
                nsamples += self.nframesamples * c.nchannels;
            }
        }

        if nsamples == 0 {
            for band in &mut c.bands[..c.nfreqbands] {
                band.lsb_off = [None; CHANNELS_MAX];
            }
            return;
        }

        if c.lsb.len() < nsamples {
            c.lsb.resize(nsamples, 0);
        }

        let mut off = 0;
        for band in 0..c.nfreqbands {
            for ch in 0..c.nchannels {
                if c.bands[band].lsb_section_size != 0 {
                    c.bands[band].lsb_off[ch] = Some(off);
                    off += self.nframesamples;
                }
                else {
                    c.bands[band].lsb_off[ch] = None;
                }
            }
        }
    }

    fn chs_parse_band_data(
        &mut self,
        gb: &mut BitReader<'_>,
        ci: usize,
        band: usize,
        seg: usize,
        band_data_end: usize,
    ) -> XResult<()> {
        let nsegsamples = self.nsegsamples;
        let nsegsamples_log2 = self.nsegsamples_log2;
        let c = &mut self.chset[ci];
        let nchannels = c.nchannels;

        // Start unpacking MSB portion of the segment.
        if !(seg != 0 && gb.get1()) {
            // Unpack segment type: 0 - distinct coding parameters for each channel, 1 - common
            // coding parameters for all channels.
            c.seg_common = gb.get1();

            // Determine number of coding parameters encoded in segment.
            let k = if c.seg_common { 1 } else { nchannels };

            // Unpack Rice coding parameters.
            for i in 0..k {
                // Unpack Rice coding flag: 0 - linear code, 1 - Rice code.
                c.rice_code_flag[i] = gb.get1();
                // Unpack Hybrid Rice coding flag: 0 - Rice code, 1 - Hybrid Rice code.
                if !c.seg_common && c.rice_code_flag[i] && gb.get1() {
                    // Unpack binary code length for isolated samples.
                    c.bitalloc_hybrid_linear[i] = gb.get(c.nabits) + 1;
                }
                else {
                    // 0 indicates no Hybrid Rice coding.
                    c.bitalloc_hybrid_linear[i] = 0;
                }
            }

            // Unpack coding parameters.
            for i in 0..k {
                if seg == 0 {
                    // Unpack coding parameter for part A of segment 0.
                    c.bitalloc_part_a[i] = gb.get(c.nabits);

                    // Adjust for the linear code.
                    if !c.rice_code_flag[i] && c.bitalloc_part_a[i] != 0 {
                        c.bitalloc_part_a[i] += 1;
                    }

                    c.nsamples_part_a[i] = if !c.seg_common {
                        c.bands[band].adapt_pred_order[i]
                    }
                    else {
                        c.bands[band].highest_pred_order
                    };
                }
                else {
                    c.bitalloc_part_a[i] = 0;
                    c.nsamples_part_a[i] = 0;
                }

                // Unpack coding parameter for part B of segment.
                c.bitalloc_part_b[i] = gb.get(c.nabits);

                // Adjust for the linear code.
                if !c.rice_code_flag[i] && c.bitalloc_part_b[i] != 0 {
                    c.bitalloc_part_b[i] += 1;
                }
            }
        }

        // Unpack entropy codes.
        for i in 0..nchannels {
            // Select index of coding parameters.
            let k = if c.seg_common { 0 } else { i };

            // Slice the segment into parts A and B.
            let na = c.nsamples_part_a[k];
            if na > nsegsamples {
                return Err(XllError::Invalid);
            }
            let start = c.bands[band].msb_off[i] + seg * nsegsamples;
            let seg_buf = &mut c.msb[start..start + nsegsamples];
            let (part_a, part_b) = seg_buf.split_at_mut(na);
            let nsamples_part_b = nsegsamples - na;

            if gb.left() < 0 {
                return Err(XllError::Invalid);
            }

            if !c.rice_code_flag[k] {
                // Linear codes. Unpack all residuals of part A of segment 0, then part B of
                // segment 0 and others.
                get_linear_array(gb, part_a, c.bitalloc_part_a[k]);
                get_linear_array(gb, part_b, c.bitalloc_part_b[k]);
            }
            else {
                // Rice codes. Unpack all residuals of part A of segment 0.
                get_rice_array(gb, part_a, c.bitalloc_part_a[k])?;

                if c.bitalloc_hybrid_linear[k] != 0 {
                    // Hybrid Rice codes. Unpack the number of isolated samples.
                    let nisosamples = gb.get(nsegsamples_log2);

                    // Set all locations to 0.
                    part_b.fill(0);

                    // Extract the locations of isolated samples and flag by -1.
                    for _ in 0..nisosamples {
                        let loc = gb.get(nsegsamples_log2) as usize;
                        if loc >= nsamples_part_b {
                            return Err(XllError::Invalid);
                        }
                        part_b[loc] = -1;
                    }

                    // Unpack all residuals of part B of segment 0 and others.
                    for p in part_b.iter_mut() {
                        *p = if *p != 0 {
                            get_linear(gb, c.bitalloc_hybrid_linear[k])
                        }
                        else {
                            get_rice(gb, c.bitalloc_part_b[k])
                        };
                    }
                }
                else {
                    // Rice codes. Unpack all residuals of part B of segment 0 and others.
                    get_rice_array(gb, part_b, c.bitalloc_part_b[k])?;
                }
            }
        }

        // Unpack decimator history for frequency band 1.
        if seg == 0 && band == 1 {
            let nbits = gb.get(5) + 1;
            for i in 0..nchannels {
                for j in 1..DECI_HISTORY_MAX {
                    c.deci_history[i][j] = gb.get_signed(nbits);
                }
            }
        }

        // Start unpacking LSB portion of the segment.
        let lsb_section_size = c.bands[band].lsb_section_size;
        if lsb_section_size != 0 {
            // Skip to the start of LSB portion.
            let lsb_start =
                band_data_end.checked_sub(lsb_section_size * 8).ok_or(XllError::Invalid)?;
            if !gb.seek(lsb_start) {
                return Err(XllError::Invalid);
            }

            // Unpack all LSB parts of residuals of this segment.
            for i in 0..nchannels {
                let nbits = c.bands[band].nscalablelsbs[i];
                if nbits != 0 {
                    let off = c.bands[band].lsb_off[i].ok_or(XllError::Invalid)?;
                    let start = off + seg * nsegsamples;
                    get_array(gb, &mut c.lsb[start..start + nsegsamples], nbits);
                }
            }
        }

        // Skip to the end of band data.
        if !gb.seek(band_data_end) {
            return Err(XllError::Invalid);
        }

        Ok(())
    }

    /// Clear band data of one segment, or of all the frame if `seg` is `None`.
    fn chs_clear_band_data(&mut self, ci: usize, band: usize, seg: Option<usize>) {
        let (offset, nsamples) = match seg {
            None => (0, self.nframesamples),
            Some(seg) => (seg * self.nsegsamples, self.nsegsamples),
        };

        let c = &mut self.chset[ci];
        for i in 0..c.nchannels {
            let start = c.bands[band].msb_off[i] + offset;
            c.msb[start..start + nsamples].fill(0);
            if c.bands[band].lsb_section_size != 0 {
                if let Some(off) = c.bands[band].lsb_off[i] {
                    c.lsb[off + offset..off + offset + nsamples].fill(0);
                }
            }
        }

        if seg.is_none_or(|s| s == 0) && band != 0 {
            c.deci_history = [[0; DECI_HISTORY_MAX]; CHANNELS_MAX];
        }

        if seg.is_none() {
            let b = &mut c.bands[band];
            b.nscalablelsbs = [0; CHANNELS_MAX];
            b.bit_width_adjust = [0; CHANNELS_MAX];
        }
    }

    fn parse_band_data(&mut self, gb: &mut BitReader<'_>) -> XResult<()> {
        for ci in 0..self.nactivechsets {
            self.chs_alloc_msb_band_data(ci);
            self.chs_alloc_lsb_band_data(ci);
        }

        let mut navi_pos = gb.pos();
        let mut navi_idx = 0;
        for band in 0..self.nfreqbands {
            for seg in 0..self.nframesegs {
                for chs in 0..self.nchsets {
                    if self.chset[chs].nfreqbands > band {
                        navi_pos += self.navi[navi_idx] * 8;
                        if navi_pos > gb.size_in_bits() {
                            return Err(XllError::Invalid);
                        }
                        if chs < self.nactivechsets
                            && self.chs_parse_band_data(gb, chs, band, seg, navi_pos).is_err()
                        {
                            self.chs_clear_band_data(chs, band, Some(seg));
                        }
                        gb.set_pos(navi_pos);
                    }
                    navi_idx += 1;
                }
            }
        }

        Ok(())
    }

    fn parse_frame(&mut self, data: &[u8], asset: &ExssAsset) -> XResult<()> {
        let mut gb = BitReader::new(data);
        self.parse_common_header(&mut gb)?;
        self.parse_sub_headers(&mut gb, asset)?;
        self.parse_navi_table(&mut gb)?;
        self.parse_band_data(&mut gb)?;

        if !gb.seek(self.frame_size * 8) {
            return Err(XllError::Invalid);
        }

        Ok(())
    }

    fn copy_to_pbr(&mut self, data: &[u8], delay: u32) -> XResult<()> {
        if data.len() > PBR_BUFFER_MAX {
            return Err(XllError::Invalid);
        }

        if self.pbr_buffer.is_empty() {
            self.pbr_buffer = vec![0; PBR_BUFFER_MAX];
        }

        self.pbr_buffer[..data.len()].copy_from_slice(data);
        self.pbr_length = data.len();
        self.pbr_delay = delay;
        Ok(())
    }

    fn parse_frame_no_pbr(&mut self, data: &[u8], asset: &ExssAsset) -> XResult<()> {
        let mut data = data;
        let mut ret = self.parse_frame(data, asset);

        // If XLL packet data didn't start with a sync word, we must have jumped right into the
        // middle of PBR smoothing period.
        if ret == Err(XllError::Again)
            && asset.xll_sync_present
            && asset.xll_sync_offset < data.len()
        {
            // Skip to the next sync word in this packet.
            data = &data[asset.xll_sync_offset..];

            // If decoding delay is set, put the frame into PBR buffer and return failure code.
            // Higher level decoder is expected to switch to lossy core decoding or mute its
            // output until decoding delay expires.
            if asset.xll_delay_nframes > 0 {
                self.copy_to_pbr(data, asset.xll_delay_nframes)?;
                return Err(XllError::Again);
            }

            // No decoding delay, just parse the frame in place.
            ret = self.parse_frame(data, asset);
        }

        ret?;

        if self.frame_size > data.len() {
            return Err(XllError::Invalid);
        }

        // If the XLL decoder didn't consume full packet, start PBR smoothing period.
        if self.frame_size < data.len() {
            self.copy_to_pbr(&data[self.frame_size..], 0)?;
        }

        Ok(())
    }

    fn parse_frame_pbr(&mut self, data: &[u8], asset: &ExssAsset) -> XResult<()> {
        let ret = self.parse_frame_pbr_inner(data, asset);
        if ret.is_err() {
            // For now, throw out all PBR state on failure.
            self.clear_pbr();
        }
        ret
    }

    fn parse_frame_pbr_inner(&mut self, data: &[u8], asset: &ExssAsset) -> XResult<()> {
        if data.len() > PBR_BUFFER_MAX - self.pbr_length {
            return Err(XllError::Invalid);
        }

        let end = self.pbr_length + data.len();
        self.pbr_buffer[self.pbr_length..end].copy_from_slice(data);
        self.pbr_length = end;

        // Respect decoding delay after synchronization error.
        if self.pbr_delay > 0 {
            self.pbr_delay -= 1;
            if self.pbr_delay != 0 {
                return Err(XllError::Again);
            }
        }

        // Borrow the buffer out of self while parsing.
        let buf = std::mem::take(&mut self.pbr_buffer);
        let ret = self.parse_frame(&buf[..self.pbr_length], asset);
        self.pbr_buffer = buf;
        ret?;

        if self.frame_size > self.pbr_length {
            return Err(XllError::Invalid);
        }

        if self.frame_size == self.pbr_length {
            // End of PBR smoothing period.
            self.clear_pbr();
        }
        else {
            self.pbr_length -= self.frame_size;
            self.pbr_buffer.copy_within(self.frame_size..self.frame_size + self.pbr_length, 0);
        }

        Ok(())
    }

    /// Parse the XLL component of an extension substream. `exss` is the substream data the asset
    /// offsets are relative to.
    pub fn parse(&mut self, exss: &[u8], asset: &ExssAsset) -> Result<(), XllError> {
        if self.hd_stream_id != asset.hd_stream_id {
            self.clear_pbr();
            self.hd_stream_id = asset.hd_stream_id;
        }

        let end = asset.xll_offset.checked_add(asset.xll_size).ok_or(XllError::Invalid)?;
        let data = exss.get(asset.xll_offset..end).ok_or(XllError::Invalid)?;

        if self.pbr_length != 0 {
            self.parse_frame_pbr(data, asset)
        }
        else {
            self.parse_frame_no_pbr(data, asset)
        }
    }

    // ---------------------------------------------------------------------------------------
    // Reconstruction
    // ---------------------------------------------------------------------------------------

    fn chs_filter_band_data(&mut self, ci: usize, band: usize) {
        let nsamples = self.nframesamples;
        let c = &mut self.chset[ci];
        let nchannels = c.nchannels;
        let b = &mut c.bands[band];
        let msb = &mut c.msb;

        // Inverse adaptive or fixed prediction.
        for i in 0..nchannels {
            let off = b.msb_off[i];
            let buf = &mut msb[off..off + nsamples];
            let order = b.adapt_pred_order[i];
            if order > 0 {
                let mut coeff = [0i32; ADAPT_PRED_ORDER_MAX];

                // Conversion from reflection coefficients to direct form coefficients.
                for j in 0..order {
                    let rc = b.adapt_refl_coeff[i][j];
                    for k in 0..j.div_ceil(2) {
                        let tmp1 = coeff[k];
                        let tmp2 = coeff[j - k - 1];
                        coeff[k] = tmp1.wrapping_add(mul16(rc, tmp2));
                        coeff[j - k - 1] = tmp2.wrapping_add(mul16(rc, tmp1));
                    }
                    coeff[j] = rc;
                }

                // Inverse adaptive prediction.
                for j in 0..nsamples.saturating_sub(order) {
                    let mut err = 0i64;
                    for k in 0..order {
                        err = err
                            .wrapping_add(i64::from(buf[j + k]) * i64::from(coeff[order - k - 1]));
                    }
                    buf[j + order] = buf[j + order].wrapping_sub(clip23(norm(err, 16)));
                }
            }
            else {
                // Inverse fixed coefficient prediction.
                for _ in 0..b.fixed_pred_order[i] {
                    for k in 1..nsamples {
                        buf[k] = buf[k].wrapping_add(buf[k - 1]);
                    }
                }
            }
        }

        // Inverse pairwise channel decorrelation.
        if b.decor_enabled {
            for i in 0..nchannels / 2 {
                let coeff = b.decor_coeff[i];
                if coeff != 0 {
                    let dst = b.msb_off[i * 2 + 1];
                    let src = b.msb_off[i * 2];
                    for n in 0..nsamples {
                        let v = msb[src + n].wrapping_mul(coeff).wrapping_add(1 << 2) >> 3;
                        msb[dst + n] = msb[dst + n].wrapping_add(v);
                    }
                }
            }

            // Reorder channel pointers to the original order.
            let tmp = b.msb_off;
            for i in 0..nchannels {
                b.msb_off[b.orig_order[i]] = tmp[i];
            }
        }

        // Map output channel pointers for frequency band 0.
        if c.nfreqbands == 1 {
            for i in 0..nchannels {
                self.output_loc[c.ch_remap[i]] = Some(Loc::Msb(ci, b.msb_off[i]));
            }
        }
    }

    fn chs_get_lsb_width(&self, ci: usize, band: usize, ch: usize) -> i32 {
        let b = &self.chset[ci].bands[band];
        let adj = b.bit_width_adjust[ch] as i32;
        let mut shift = b.nscalablelsbs[ch] as i32;

        if self.fixed_lsb_width != 0 {
            shift = self.fixed_lsb_width;
        }
        else if shift != 0 && adj != 0 {
            shift += adj - 1;
        }
        else {
            shift += adj;
        }

        shift
    }

    fn chs_assemble_msbs_lsbs(&mut self, ci: usize, band: usize) {
        let nsamples = self.nframesamples;

        for ch in 0..self.chset[ci].nchannels {
            let shift = self.chs_get_lsb_width(ci, band, ch);
            if shift == 0 {
                continue;
            }

            let c = &mut self.chset[ci];
            let b = &c.bands[band];
            let mul = 1i32.wrapping_shl(shift as u32);
            let moff = b.msb_off[ch];
            let msb = &mut c.msb[moff..moff + nsamples];

            match b.lsb_off[ch] {
                Some(loff) if b.nscalablelsbs[ch] != 0 => {
                    let adj = b.bit_width_adjust[ch];
                    let lsb = &c.lsb[loff..loff + nsamples];
                    for (m, &l) in msb.iter_mut().zip(lsb) {
                        *m = m.wrapping_mul(mul).wrapping_add(l.wrapping_shl(adj));
                    }
                }
                _ => {
                    for m in msb.iter_mut() {
                        *m = m.wrapping_mul(mul);
                    }
                }
            }
        }
    }

    fn chs_assemble_freq_bands(&mut self, ci: usize) {
        let nsamples = self.nframesamples;
        let c = &mut self.chset[ci];

        if c.assembled.len() < 2 * nsamples * c.nchannels {
            c.assembled.resize(2 * nsamples * c.nchannels, 0);
        }

        // Assemble frequency bands 0 and 1.
        for ch in 0..c.nchannels {
            let b0 = c.bands[0].msb_off[ch];
            let b1 = c.bands[1].msb_off[ch];

            // Copy decimator history.
            c.msb[b0 - DECI_HISTORY_MAX..b0].copy_from_slice(&c.deci_history[ch]);

            // Filter.
            let out = &mut c.assembled[ch * nsamples * 2..(ch + 1) * nsamples * 2];
            assemble_freq_bands(out, &mut c.msb, b0, b1, nsamples);

            // Remap output channel pointer to assembly buffer.
            self.output_loc[c.ch_remap[ch]] = Some(Loc::Assembled(ci, ch * nsamples * 2));
        }
    }

    fn undo_down_mix(&mut self, oi: usize, band: usize) {
        let nframesamples = self.nframesamples;
        let nactivechsets = self.nactivechsets;
        let (left, right) = self.chset.split_at_mut(oi);
        let o = &right[0];

        let mut nchannels = 0;
        let mut coeff_ptr = 0;
        for c in &mut left[..nactivechsets.min(oi)] {
            if !c.hier_chset {
                continue;
            }

            for j in 0..c.nchannels {
                for k in 0..o.nchannels {
                    let coeff = o.dmix_coeff[coeff_ptr];
                    coeff_ptr += 1;
                    if coeff != 0 {
                        let dst = c.bands[band].msb_off[j];
                        let src = o.bands[band].msb_off[k];
                        for n in 0..nframesamples {
                            c.msb[dst + n] =
                                c.msb[dst + n].wrapping_sub(mul15(o.msb[src + n], coeff));
                        }
                        if band != 0 {
                            for n in 0..DECI_HISTORY_MAX {
                                c.deci_history[j][n] = c.deci_history[j][n]
                                    .wrapping_sub(mul15(o.deci_history[k][n], coeff));
                            }
                        }
                    }
                }
            }

            nchannels += c.nchannels;
            if nchannels >= o.hier_ofs {
                break;
            }
        }
    }

    fn scale_down_mix(&mut self, oi: usize, band: usize) {
        let nframesamples = self.nframesamples;
        let nactivechsets = self.nactivechsets;
        let scales = self.chset[oi].dmix_scale;
        let hier_ofs = self.chset[oi].hier_ofs;

        let mut nchannels = 0;
        for c in &mut self.chset[..nactivechsets] {
            if !c.hier_chset {
                continue;
            }

            for j in 0..c.nchannels {
                let scale = scales[nchannels];
                nchannels += 1;
                if scale != (1 << 15) {
                    let dst = c.bands[band].msb_off[j];
                    for v in &mut c.msb[dst..dst + nframesamples] {
                        *v = mul15(*v, scale);
                    }
                    if band != 0 {
                        for v in &mut c.deci_history[j] {
                            *v = mul15(*v, scale);
                        }
                    }
                }
            }

            if nchannels >= hier_ofs {
                break;
            }
        }
    }

    /// Clear all band data and replace non-residual encoded channels with lossy counterparts.
    fn force_lossy_output(&mut self, ci: usize, core: &CoreDecoder) {
        for band in 0..self.chset[ci].nfreqbands {
            self.chs_clear_band_data(ci, band, None);
        }

        let c = &mut self.chset[ci];
        for ch in 0..c.nchannels {
            if c.residual_encode & (1 << ch) == 0 {
                continue;
            }
            if core.map_spkr(c.ch_remap[ch]).is_none() {
                continue;
            }
            c.residual_encode &= !(1 << ch);
        }
    }

    fn combine_residual_frame(&mut self, ci: usize, core: Option<&CoreDecoder>) -> XResult<()> {
        let nsamples = self.nframesamples;

        // Verify that core is compatible.
        let Some(core) = core
        else {
            // Residual encoded channels are present without core.
            return Err(XllError::Hard);
        };

        if self.chset[ci].freq != core.output_rate {
            return Err(XllError::Invalid);
        }

        if nsamples != core.npcmsamples {
            return Err(XllError::Invalid);
        }

        // See if this channel set is downmixed and find the next channel set in hierarchy. If
        // downmixed, undo core pre-scaling before combining with residual (residual is not
        // scaled).
        let o = self.find_next_hier_dmix_chset(ci).map(|oi| self.chset[oi].dmix_scale_inv);

        // Reduce core bit width and combine with residual.
        for ch in 0..self.chset[ci].nchannels {
            let c = &self.chset[ci];
            if c.residual_encode & (1 << ch) != 0 {
                continue;
            }

            // Map this channel to core speaker.
            let Some(spkr) = core.map_spkr(c.ch_remap[ch])
            else {
                return Err(XllError::Invalid);
            };

            // Account for LSB width.
            let shift = 24 - c.pcm_bit_res as i32 + self.chs_get_lsb_width(ci, 0, ch);
            if shift > 24 {
                return Err(XllError::Invalid);
            }

            let round = if shift > 0 { 1i32 << (shift - 1) } else { 0 };
            let shift = shift.max(0) as u32;

            let src = &core.out_fixed[spkr][..nsamples];
            let hier_ofs = c.hier_ofs;
            let dst_off = c.bands[0].msb_off[ch];
            let dst = &mut self.chset[ci].msb[dst_off..dst_off + nsamples];

            if let Some(scale_inv) = o {
                // Undo embedded core downmix pre-scaling.
                let scale_inv = scale_inv[hier_ofs + ch];
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = d.wrapping_add(clip23(mul16(s, scale_inv).wrapping_add(round) >> shift));
                }
            }
            else {
                // No downmix scaling.
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = d.wrapping_add(s.wrapping_add(round) >> shift);
                }
            }
        }

        Ok(())
    }

    /// Reconstruct the PCM of the current frame.
    ///
    /// `core` is the core decoder, if the packet has a core; it must have been filtered with
    /// [`CoreDecoder::filter_fixed`] already. If `recovery` is set, lossy output of the core is
    /// forced.
    pub fn filter_frame(
        &mut self,
        core: Option<&CoreDecoder>,
        recovery: bool,
    ) -> Result<XllFrame, XllError> {
        // Force lossy downmixed output during recovery.
        if recovery {
            if let Some(core) = core {
                for i in 0..self.nchsets {
                    if i < self.nactivechsets {
                        self.force_lossy_output(i, core);
                    }

                    if !self.chset[i].primary_chset {
                        self.chset[i].dmix_embedded = false;
                    }
                }
            }

            self.scalable_lsbs = false;
            self.fixed_lsb_width = 0;
        }

        // Filter frequency bands for active channel sets.
        self.output_mask = 0;
        for i in 0..self.nactivechsets {
            self.chs_filter_band_data(i, 0);

            let c = &self.chset[i];
            if c.residual_encode != (1u32 << c.nchannels) - 1 {
                self.combine_residual_frame(i, core)?;
            }

            if self.scalable_lsbs {
                self.chs_assemble_msbs_lsbs(i, 0);
            }

            if self.chset[i].nfreqbands > 1 {
                self.chs_filter_band_data(i, 1);
                self.chs_assemble_msbs_lsbs(i, 1);
            }

            self.output_mask |= self.chset[i].ch_mask;
        }

        // Undo hierarchial downmix and/or apply scaling.
        for i in 1..self.nchsets {
            if !self.chset[i].is_hier_dmix() {
                continue;
            }

            let nfreqbands = self.chset[i].nfreqbands;
            if i >= self.nactivechsets {
                for j in 0..nfreqbands {
                    if self.chset[i].bands[j].dmix_embedded {
                        self.scale_down_mix(i, j);
                    }
                }
                break;
            }

            for j in 0..nfreqbands {
                if self.chset[i].bands[j].dmix_embedded {
                    self.undo_down_mix(i, j);
                }
            }
        }

        // Assemble frequency bands for active channel sets.
        if self.nfreqbands > 1 {
            for i in 0..self.nactivechsets {
                self.chs_assemble_freq_bands(i);
            }
        }

        let p = &self.chset[0];
        let nfreqbands = self.nfreqbands.max(1);
        let sample_rate = p.freq << (p.nfreqbands - 1);
        let (bits, shift) = match p.storage_bit_res {
            16 => (16, 16 - p.pcm_bit_res),
            20 | 24 => (24, 24 - p.pcm_bit_res),
            _ => return Err(XllError::Hard),
        };

        let nsamples = self.nframesamples << (nfreqbands - 1);
        let mask = self.output_mask;

        for sp in 0..spkr::COUNT {
            if mask & (1 << sp) == 0 {
                continue;
            }

            let Some(loc) = self.output_loc[sp]
            else {
                return Err(XllError::Hard);
            };

            let samples = match loc {
                Loc::Msb(ci, off) => self.chset[ci].msb.get(off..off + nsamples),
                Loc::Assembled(ci, off) => self.chset[ci].assembled.get(off..off + nsamples),
            };
            let Some(samples) = samples
            else {
                return Err(XllError::Hard);
            };

            let out = &mut self.out[sp];
            out.clear();
            if bits == 16 {
                out.extend(
                    samples
                        .iter()
                        .map(|&s| s.wrapping_mul(1i32.wrapping_shl(shift)).clamp(-32768, 32767)),
                );
            }
            else {
                out.extend(
                    samples.iter().map(|&s| clip23(s.wrapping_mul(1i32.wrapping_shl(shift)))),
                );
            }
        }

        Ok(XllFrame { nsamples, sample_rate, ch_mask: mask, bits })
    }
}

fn ceil_log2(v: u32) -> u32 {
    if v <= 1 { 0 } else { 32 - (v - 1).leading_zeros() }
}

/// `dst[n] -= mulN(src[n], coeff)` where both vectors are ranges of `buf`.
fn filter_band(buf: &mut [i32], dst: usize, src: usize, coeff: i32, len: usize, bits: i32) {
    for n in 0..len {
        let s = i64::from(buf[src + n]) * i64::from(coeff);
        buf[dst + n] = buf[dst + n].wrapping_sub(norm(s, bits));
    }
}

/// Assemble two frequency bands (`band0` has `DECI_HISTORY_MAX` samples of history before
/// `b0`) into one signal of twice the sample rate (`dcadsp.c: assemble_freq_bands_c`).
fn assemble_freq_bands(dst: &mut [i32], buf: &mut [i32], b0: usize, b1: usize, len: usize) {
    let coeff = &XLL_BAND_COEFF;

    filter_band(buf, b0, b1, coeff[0], len, 22);
    filter_band(buf, b1, b0, coeff[1], len, 22);
    filter_band(buf, b0, b1, coeff[2], len, 22);
    filter_band(buf, b1, b0, coeff[3], len, 22);

    let mut p0 = b0;
    for i in 0..8 {
        filter_band(buf, p0, b1, coeff[i + 4], len, 23);
        filter_band(buf, b1, p0, coeff[i + 12], len, 23);
        filter_band(buf, p0, b1, coeff[i + 4], len, 23);
        p0 -= 1;
    }

    for i in 0..len {
        dst[2 * i] = buf[b1 + i];
        dst[2 * i + 1] = buf[p0 + 1 + i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::XLL_REFL_COEFF;

    /// MSB-first bit writer.
    #[derive(Default)]
    struct BitWriter {
        buf: Vec<u8>,
        nbits: usize,
    }

    impl BitWriter {
        fn put(&mut self, n: u32, v: u32) {
            for i in (0..n).rev() {
                if self.nbits % 8 == 0 {
                    self.buf.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                *self.buf.last_mut().unwrap() |= bit << (7 - self.nbits % 8);
                self.nbits += 1;
            }
        }

        fn align(&mut self) {
            self.nbits = self.buf.len() * 8;
        }
    }

    /// Zig-zag code a sample for the linear code.
    fn linear(s: i32) -> u32 {
        if s >= 0 { (s as u32) << 1 } else { ((-s) as u32) * 2 - 1 }
    }

    const NSAMPLES: usize = 16;
    // Code length of a residual - 1.
    const PART_BITS: u32 = 12;

    /// Build a single frame, single channel set, single full-band channel, 24-bit XLL frame with
    /// linear coded 16-sample segment. `adapt` selects a 1st order adaptive predictor with the
    /// given reflection coefficient index, otherwise the fixed predictor order `fixed` is used.
    fn build_frame(residual: &[i32; NSAMPLES], adapt: Option<u32>, fixed: u32) -> Vec<u8> {
        // Band data.
        let mut band = BitWriter::default();
        band.put(1, 0); // Distinct coding parameters.
        band.put(1, 0); // Linear code.
        band.put(5, PART_BITS); // Part A coding parameter.
        band.put(5, PART_BITS); // Part B coding parameter.
        for &r in residual {
            band.put(PART_BITS + 1, linear(r));
        }
        band.align();

        // Channel set sub-header.
        let mut sub = BitWriter::default();
        sub.put(10, 0); // Header size, patched later.
        sub.put(4, 0); // 1 channel.
        sub.put(1, 1); // Full channel.
        sub.put(5, 23); // 24-bit PCM.
        sub.put(5, 23); // 24-bit storage.
        sub.put(4, 12); // 48 kHz.
        sub.put(2, 0);
        sub.put(2, 0);
        sub.put(1, 1); // Primary.
        sub.put(1, 0); // No downmix coefficients.
        sub.put(1, 1); // Hierarchy.
        sub.put(1, 1); // Channel mask enabled.
        sub.put(8, 1); // Center.
        sub.put(1, 0); // No decorrelation.
        match adapt {
            Some(k) => {
                sub.put(4, 1);
                sub.put(8, k << 1);
            }
            None => {
                sub.put(4, 0);
                sub.put(2, fixed);
            }
        }
        sub.align();
        sub.put(16, 0); // CRC.
        let sub_len = sub.buf.len();
        sub.buf[0] = ((sub_len - 1) >> 2) as u8;
        sub.buf[1] |= (((sub_len - 1) & 3) << 6) as u8;

        // NAVI table.
        let mut navi = BitWriter::default();
        navi.put(16, band.buf.len() as u32 - 1);
        navi.put(16, 0); // CRC.

        let frame_len = 14 + sub_len + navi.buf.len() + band.buf.len();

        // Common header.
        let mut hdr = BitWriter::default();
        hdr.put(32, SYNC_XLL);
        hdr.put(4, 0);
        hdr.put(8, 13); // 14 bytes.
        hdr.put(5, 15);
        hdr.put(16, frame_len as u32 - 1);
        hdr.put(4, 0); // 1 channel set.
        hdr.put(4, 0); // 1 segment.
        hdr.put(4, 4); // 16 samples.
        hdr.put(5, 15);
        hdr.put(2, 0);
        hdr.put(1, 0);
        hdr.put(5, 7);
        hdr.align();
        hdr.put(16, 0); // CRC.
        assert_eq!(hdr.buf.len(), 14);

        let mut out = hdr.buf;
        out.extend_from_slice(&sub.buf);
        out.extend_from_slice(&navi.buf);
        out.extend_from_slice(&band.buf);
        out
    }

    fn decode(data: &[u8]) -> (XllDecoder, Result<XllFrame, XllError>) {
        let asset = ExssAsset {
            one_to_one_map_ch_to_spkr: true,
            xll_size: data.len(),
            ..Default::default()
        };
        let mut dec = XllDecoder::new();
        dec.parse(data, &asset).unwrap();
        let frame = dec.filter_frame(None, false);
        (dec, frame)
    }

    const PCM: [i32; NSAMPLES] =
        [0, 5, -7, 120, 900, 1500, 2047, 1000, -300, -1900, -2047, -1000, 3, 3, 3, 2000];

    #[test]
    fn linear_fixed_prediction() {
        for order in 0..=3u32 {
            // The encoder takes the `order`th difference of the PCM.
            let mut res = PCM;
            for _ in 0..order {
                for i in (1..NSAMPLES).rev() {
                    res[i] -= res[i - 1];
                }
            }

            let (dec, frame) = decode(&build_frame(&res, None, order));
            let frame = frame.unwrap();

            assert_eq!((frame.nsamples, frame.sample_rate, frame.bits), (NSAMPLES, 48000, 24));
            assert_eq!(frame.ch_mask, 1 << spkr::C);
            assert_eq!(dec.out[spkr::C], PCM, "fixed order {order}");
        }
    }

    #[test]
    fn linear_adaptive_prediction() {
        let k = 20;
        let coeff = i32::from(XLL_REFL_COEFF[k as usize]);

        // The encoder adds the prediction of the 1st order predictor to the PCM. The first sample
        // has no history.
        let mut res = PCM;
        for i in 1..NSAMPLES {
            res[i] = PCM[i] + clip23(norm(i64::from(PCM[i - 1]) * i64::from(coeff), 16));
        }

        let (dec, frame) = decode(&build_frame(&res, Some(k), 0));
        frame.unwrap();
        assert_eq!(dec.out[spkr::C], PCM);
    }

    #[test]
    fn invalid_frames() {
        let asset =
            ExssAsset { one_to_one_map_ch_to_spkr: true, xll_size: 0, ..Default::default() };
        let good = build_frame(&PCM, None, 0);

        // Not a sync word.
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        let a = ExssAsset { xll_size: bad.len(), ..asset.clone() };
        assert_eq!(XllDecoder::new().parse(&bad, &a), Err(XllError::Again));

        // Truncated at every length.
        for len in 0..good.len() {
            let a = ExssAsset { xll_size: len, ..asset.clone() };
            assert!(XllDecoder::new().parse(&good[..len], &a).is_err(), "len {len}");
        }

        // Corrupt every byte of the headers, there must not be a panic.
        for i in 0..good.len() {
            for flip in [0x01, 0x10, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[i] ^= flip;
                let a = ExssAsset { xll_size: bad.len(), ..asset.clone() };
                let mut dec = XllDecoder::new();
                if dec.parse(&bad, &a).is_ok() {
                    let _ = dec.filter_frame(None, false);
                }
            }
        }
    }
}
