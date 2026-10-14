// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-substream decoding core: audio frame and audio block parsing, mantissa
//! reconstruction, coupling, rematrixing, and the inverse transform.
//!
//! One [`Core`] decodes one (independent or dependent) substream. Channel index `0` always
//! refers to the coupling channel, indices `1..=fbw` to the full bandwidth channels in coded
//! order, and index `fbw + 1` to the LFE channel (if present).

use std::sync::LazyLock;

use crate::bitalloc::{self, DeltaMode, DeltaSeg, Params};
use crate::bits::BitReader;
use crate::header::FrameHeader;
use crate::imdct::{BLOCK_SIZE, transforms};
use crate::tables::{
    BAP_TAB, DB_PER_BIT, FAST_DECAY, FAST_GAIN, FLOOR, FRM_EXPSTR, HEBAP_TAB, LFG_INIT, SLOW_DECAY,
    SLOW_GAIN,
};
use crate::transient::{FrameData, MAX_FBW, PreNoise};

/// The coupling channel index.
pub const CPL: usize = 0;

/// The maximum number of channel slots (coupling + 5 fbw + LFE).
pub const MAX_CH: usize = 7;

/// The maximum number of audio blocks per syncframe.
pub const MAX_BLOCKS: usize = 6;

/// Exponent strategies.
pub const EXP_REUSE: u8 = 0;

/// Rematrixing band edges.
const REMAT_BANDS: [usize; 5] = [13, 25, 37, 61, 253];

/// Mantissa quantizer bit widths for `bap` 6 to 15.
const QUANTIZATION_BITS: [u32; 16] = [0, 3, 5, 7, 11, 15, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// Default band structures.
const DEFAULT_CPL_BAND_STRUCT: [u8; 18] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1];

/// Dynamic range scale factors indexed by the 8-bit `dynrng` code.
static DYNAMIC_RANGE: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut t = [0.0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let e = (i >> 5) as i32 - ((i >> 7) << 3) as i32 - 5;
        *v = 2.0f32.powi(e) * ((i & 0x1f) | 0x20) as f32;
    }
    t
});

/// A bit stream error; the remainder of the syncframe cannot be decoded.
#[derive(Debug)]
pub struct Invalid;

pub type Res<T = ()> = Result<T, Invalid>;

/// A lagged Fibonacci pseudo-random generator, used as the source of noise for zero-bit
/// mantissas and spectral extension.
pub struct Lfg {
    state: [u32; 64],
    index: usize,
}

impl Lfg {
    pub fn new() -> Self {
        Lfg { state: LFG_INIT, index: 0 }
    }

    pub fn reset(&mut self) {
        *self = Lfg::new();
    }

    #[inline]
    pub fn get(&mut self) -> u32 {
        let a = self.state[(self.index.wrapping_sub(24)) & 63]
            .wrapping_add(self.state[(self.index.wrapping_sub(55)) & 63]);
        self.state[self.index & 63] = a;
        self.index = self.index.wrapping_add(1);
        a
    }
}

impl Default for Lfg {
    fn default() -> Self {
        Lfg::new()
    }
}

/// State of the grouped mantissa decoder (bap 1, 2 and 4 share codewords between mantissas).
#[derive(Default)]
struct MantGroups {
    b1: u8,
    b1_mant: [i32; 2],
    b2: u8,
    b2_mant: [i32; 2],
    b4: u8,
    b4_mant: i32,
}

/// Symmetric dequantization of a mantissa code with the given number of levels.
#[inline]
fn symmetric_dequant(code: i32, levels: i32) -> i32 {
    ((code - (levels >> 1)) * (1 << 24)) / levels
}

pub struct Core {
    // Syncframe configuration.
    pub(crate) eac3: bool,
    pub(crate) acmod: usize,
    pub(crate) fbw: usize,
    pub(crate) lfe_on: bool,
    pub(crate) lfe_ch: usize,
    pub(crate) channels: usize,
    pub(crate) num_blocks: usize,
    pub(crate) is_independent: bool,

    // Audio frame (E-AC-3) syntax flags.
    block_switch_syntax: bool,
    dither_flag_syntax: bool,
    bit_allocation_syntax: bool,
    fast_gain_syntax: bool,
    dba_syntax: bool,
    skip_syntax: bool,
    snr_offset_strategy: u32,
    cpl_strategy_exists: [bool; MAX_BLOCKS],
    cpl_in_use_blk: [bool; MAX_BLOCKS],
    exp_strategy: [[u8; MAX_CH]; MAX_BLOCKS],
    pub(crate) channel_uses_aht: [bool; MAX_CH],
    pub(crate) spx_atten_code: [i32; MAX_CH],

    // Block state that persists from block to block.
    block_switch: [bool; MAX_CH],
    dither_flag: [bool; MAX_CH],
    dynamic_range: [f32; 2],

    pub(crate) spx_in_use: bool,
    pub(crate) spx_struct_valid: bool,
    pub(crate) channel_uses_spx: [bool; MAX_CH],
    pub(crate) first_spx_coords: [bool; MAX_CH],
    pub(crate) spx_dst_start_freq: usize,
    pub(crate) spx_src_start_freq: usize,
    pub(crate) spx_dst_end_freq: usize,
    pub(crate) spx_start_subband: usize,
    pub(crate) spx_band_struct: [u8; 17],
    pub(crate) num_spx_bands: usize,
    pub(crate) spx_band_sizes: [u8; 17],
    pub(crate) spx_noise_blend: [[f32; 17]; MAX_CH],
    pub(crate) spx_signal_blend: [[f32; 17]; MAX_CH],

    pub(crate) cpl_in_use: bool,
    pub(crate) ecpl_in_use: bool,
    cpl_struct_valid: bool,
    pub(crate) ecpl_struct_valid: bool,
    pub(crate) channel_in_cpl: [bool; MAX_CH],
    pub(crate) first_cpl_coords: [bool; MAX_CH],
    first_cpl_leak: bool,
    phase_flags_in_use: bool,
    phase_flags: [bool; 18],
    cpl_band_struct: [u8; 18],
    num_cpl_bands: usize,
    cpl_band_sizes: [u8; 18],
    cpl_coords: [[i32; 18]; MAX_CH],
    pub(crate) ecpl_begin_subbnd: usize,
    pub(crate) ecpl_end_subbnd: usize,
    pub(crate) ecpl_begf: usize,
    pub(crate) ecpl_band_struct: [u8; 22],
    pub(crate) num_ecpl_bands: usize,
    pub(crate) ecpl_band_edges: [u8; 23],
    pub(crate) ecpl_amp: [[u8; 22]; MAX_CH],

    pub(crate) start_freq: [usize; MAX_CH],
    pub(crate) end_freq: [usize; MAX_CH],
    num_exp_groups: [usize; MAX_CH],

    num_rematrixing_bands: usize,
    rematrixing_flags: [bool; 4],

    params: Params,
    snr_offset: [i32; MAX_CH],
    fast_gain: [i32; MAX_CH],
    dba_mode: [DeltaMode; MAX_CH],
    dba: [[DeltaSeg; 8]; MAX_CH],
    dba_nsegs: [usize; MAX_CH],

    // Working arrays.
    pub(crate) dexps: [[u8; 264]; MAX_CH],
    pub(crate) bap: [[u8; 256]; MAX_CH],
    psd: [[i16; 256]; MAX_CH],
    band_psd: [[i16; 64]; MAX_CH],
    mask: [[i16; 64]; MAX_CH],
    pub(crate) fixed: [[i32; 256]; MAX_CH],
    pub(crate) pre_mant: Box<[[[i32; 6]; 256]; MAX_CH]>,
    pub(crate) tc: [[f32; 256]; MAX_CH],

    // Transform state.
    delay: [[f32; BLOCK_SIZE / 2]; MAX_CH - 1],
    last_block: [[f32; BLOCK_SIZE]; MAX_CH - 1],

    /// The decoded PCM of the last syncframe: one row per coded channel (fbw channels followed
    /// by the LFE), in coded order.
    pub pcm: Box<[[f32; BLOCK_SIZE * MAX_BLOCKS]; MAX_CH - 1]>,

    /// Transient pre-noise processing state and the data of the current frame.
    pre_noise: PreNoise,
    pre_noise_data: FrameData,
}

macro_rules! bail {
    ($cond:expr) => {
        if $cond {
            return Err(Invalid);
        }
    };
}

impl Core {
    pub fn new() -> Self {
        Core {
            eac3: false,
            acmod: 0,
            fbw: 0,
            lfe_on: false,
            lfe_ch: 0,
            channels: 0,
            num_blocks: 6,
            is_independent: true,
            block_switch_syntax: true,
            dither_flag_syntax: true,
            bit_allocation_syntax: true,
            fast_gain_syntax: false,
            dba_syntax: true,
            skip_syntax: true,
            snr_offset_strategy: 2,
            cpl_strategy_exists: [false; MAX_BLOCKS],
            cpl_in_use_blk: [false; MAX_BLOCKS],
            exp_strategy: [[0; MAX_CH]; MAX_BLOCKS],
            channel_uses_aht: [false; MAX_CH],
            spx_atten_code: [-1; MAX_CH],
            block_switch: [false; MAX_CH],
            dither_flag: [false; MAX_CH],
            dynamic_range: [1.0; 2],
            spx_in_use: false,
            spx_struct_valid: false,
            channel_uses_spx: [false; MAX_CH],
            first_spx_coords: [true; MAX_CH],
            spx_dst_start_freq: 0,
            spx_src_start_freq: 0,
            spx_dst_end_freq: 0,
            spx_start_subband: 0,
            spx_band_struct: [0; 17],
            num_spx_bands: 0,
            spx_band_sizes: [0; 17],
            spx_noise_blend: [[0.0; 17]; MAX_CH],
            spx_signal_blend: [[0.0; 17]; MAX_CH],
            cpl_in_use: false,
            ecpl_in_use: false,
            cpl_struct_valid: false,
            ecpl_struct_valid: false,
            channel_in_cpl: [false; MAX_CH],
            first_cpl_coords: [true; MAX_CH],
            first_cpl_leak: false,
            phase_flags_in_use: false,
            phase_flags: [false; 18],
            cpl_band_struct: [0; 18],
            num_cpl_bands: 0,
            cpl_band_sizes: [0; 18],
            cpl_coords: [[0; 18]; MAX_CH],
            ecpl_begin_subbnd: 0,
            ecpl_end_subbnd: 0,
            ecpl_begf: 0,
            ecpl_band_struct: [0; 22],
            num_ecpl_bands: 0,
            ecpl_band_edges: [0; 23],
            ecpl_amp: [[0; 22]; MAX_CH],
            start_freq: [0; MAX_CH],
            end_freq: [0; MAX_CH],
            num_exp_groups: [0; MAX_CH],
            num_rematrixing_bands: 0,
            rematrixing_flags: [false; 4],
            params: Params::default(),
            snr_offset: [0; MAX_CH],
            fast_gain: [0; MAX_CH],
            dba_mode: [DeltaMode::None; MAX_CH],
            dba: [[DeltaSeg::default(); 8]; MAX_CH],
            dba_nsegs: [0; MAX_CH],
            dexps: [[0; 264]; MAX_CH],
            bap: [[0; 256]; MAX_CH],
            psd: [[0; 256]; MAX_CH],
            band_psd: [[0; 64]; MAX_CH],
            mask: [[0; 64]; MAX_CH],
            fixed: [[0; 256]; MAX_CH],
            pre_mant: Box::new([[[0; 6]; 256]; MAX_CH]),
            tc: [[0.0; 256]; MAX_CH],
            delay: [[0.0; BLOCK_SIZE / 2]; MAX_CH - 1],
            last_block: [[0.0; BLOCK_SIZE]; MAX_CH - 1],
            pcm: Box::new([[0.0; BLOCK_SIZE * MAX_BLOCKS]; MAX_CH - 1]),
            pre_noise: PreNoise::new(),
            pre_noise_data: [None; MAX_FBW],
        }
    }

    /// Clears all state carried between syncframes.
    pub fn reset(&mut self) {
        *self = Core::new();
    }

    /// Decodes the syncframe `data` (whose header is `hdr`) into `self.pcm`.
    ///
    /// Returns false if the frame was damaged. Damaged blocks are concealed by repeating the
    /// last good block.
    pub fn decode_frame(&mut self, hdr: &FrameHeader, data: &[u8], lfg: &mut Lfg) -> bool {
        let channels = hdr.channels();

        if hdr.muted {
            // Decoders shall mute bit stream versions they do not support.
            self.delay = [[0.0; BLOCK_SIZE / 2]; MAX_CH - 1];
            self.last_block = [[0.0; BLOCK_SIZE]; MAX_CH - 1];
            self.channels = channels;
            self.acmod = usize::from(hdr.acmod);
            for row in &mut self.pcm[..channels] {
                row[..hdr.num_blocks * BLOCK_SIZE].fill(0.0);
            }
            return true;
        }

        // A change of channel configuration invalidates the overlap and concealment state.
        if self.channels != channels || self.acmod != usize::from(hdr.acmod) {
            self.delay = [[0.0; BLOCK_SIZE / 2]; MAX_CH - 1];
            self.last_block = [[0.0; BLOCK_SIZE]; MAX_CH - 1];
            self.pre_noise.clear();
        }

        self.pre_noise_data = [None; MAX_FBW];
        self.eac3 = hdr.eac3;
        self.acmod = usize::from(hdr.acmod);
        self.fbw = hdr.fbw_channels();
        self.lfe_on = hdr.lfeon;
        self.lfe_ch = self.fbw + 1;
        self.channels = channels;
        self.num_blocks = hdr.num_blocks;
        self.is_independent = hdr.strmtyp != crate::header::STRMTYP_DEPENDENT;

        self.params.sr_code = hdr.sr_code;
        self.params.sr_shift = hdr.sr_shift;

        // The LFE channel always occupies bins 0 to 6 with two exponent groups.
        if self.lfe_on {
            self.start_freq[self.lfe_ch] = 0;
            self.end_freq[self.lfe_ch] = 7;
            self.num_exp_groups[self.lfe_ch] = 2;
            self.dither_flag[self.lfe_ch] = false;
        }
        self.dither_flag[CPL] = false;

        let mut br = BitReader::new(data);
        br.skip(hdr.header_bits);

        let mut err = false;

        if self.eac3 {
            err = self.parse_audfrm(&mut br, hdr).is_err();
        }
        else {
            self.block_switch_syntax = true;
            self.dither_flag_syntax = true;
            self.bit_allocation_syntax = true;
            self.fast_gain_syntax = false;
            self.dba_syntax = true;
            self.skip_syntax = true;
            self.snr_offset_strategy = 2;
            self.channel_uses_aht = [false; MAX_CH];
            self.spx_atten_code = [-1; MAX_CH];
            self.cpl_in_use = false;
        }

        for blk in 0..self.num_blocks {
            if !err && (self.decode_block(&mut br, blk, lfg).is_err() || br.overrun()) {
                err = true;
            }

            let range = blk * BLOCK_SIZE..(blk + 1) * BLOCK_SIZE;

            if err {
                // Conceal by repeating the last output block.
                for ch in 0..self.channels {
                    self.pcm[ch][range.clone()].copy_from_slice(&self.last_block[ch]);
                }
            }
            else {
                for ch in 0..self.channels {
                    self.last_block[ch].copy_from_slice(&self.pcm[ch][range.clone()]);
                }
            }
        }

        if self.eac3 {
            if err {
                self.pre_noise.clear();
            }
            else {
                let len = self.num_blocks * BLOCK_SIZE;
                self.pre_noise.process_frame(
                    &mut self.pcm[..],
                    self.fbw,
                    len,
                    &self.pre_noise_data,
                );
            }
        }

        !err
    }

    /// Parses the E-AC-3 audio frame element.
    fn parse_audfrm(&mut self, br: &mut BitReader<'_>, hdr: &FrameHeader) -> Res {
        let nb = self.num_blocks;
        let fbw = self.fbw;

        let (expstre, ahte) = if nb == 6 { (br.bit(), br.bit()) } else { (true, false) };

        self.snr_offset_strategy = br.get(2);
        let transproce = br.bit();

        self.block_switch_syntax = br.bit();
        if !self.block_switch_syntax {
            self.block_switch = [false; MAX_CH];
        }

        self.dither_flag_syntax = br.bit();
        if !self.dither_flag_syntax {
            for ch in 1..=fbw {
                self.dither_flag[ch] = true;
            }
        }

        self.bit_allocation_syntax = br.bit();
        if !self.bit_allocation_syntax {
            self.params.slow_decay = i32::from(SLOW_DECAY[2]);
            self.params.fast_decay = i32::from(FAST_DECAY[1]);
            self.params.slow_gain = i32::from(SLOW_GAIN[1]);
            self.params.db_per_bit = i32::from(DB_PER_BIT[2]);
            self.params.floor = i32::from(FLOOR[7]);
        }

        self.fast_gain_syntax = br.bit();
        self.dba_syntax = br.bit();
        self.skip_syntax = br.bit();
        let spxattene = br.bit();

        // Coupling strategy existence and use.
        let mut ncplblks = 0;
        if self.acmod > 1 {
            for blk in 0..nb {
                self.cpl_strategy_exists[blk] = blk == 0 || br.bit();
                self.cpl_in_use_blk[blk] = if self.cpl_strategy_exists[blk] {
                    br.bit()
                }
                else {
                    self.cpl_in_use_blk[blk - 1]
                };
                ncplblks += usize::from(self.cpl_in_use_blk[blk]);
            }
        }
        else {
            self.cpl_strategy_exists = [false; MAX_BLOCKS];
            self.cpl_in_use_blk = [false; MAX_BLOCKS];
        }

        // Exponent strategies.
        if expstre {
            for blk in 0..nb {
                for ch in usize::from(!self.cpl_in_use_blk[blk])..=fbw {
                    self.exp_strategy[blk][ch] = br.get(2) as u8;
                }
            }
        }
        else {
            let first = usize::from(!(self.acmod > 1 && ncplblks > 0));
            for ch in first..=fbw {
                let code = br.get(5) as usize;
                for blk in 0..6 {
                    self.exp_strategy[blk][ch] = FRM_EXPSTR[code][blk];
                }
            }
        }
        if self.lfe_on {
            for blk in 0..nb {
                self.exp_strategy[blk][self.lfe_ch] = br.get(1) as u8;
            }
        }

        // Converter exponent strategies.
        if hdr.strmtyp == crate::header::STRMTYP_INDEPENDENT && (nb == 6 || br.bit()) {
            br.skip(5 * fbw);
        }

        // Adaptive hybrid transform usage.
        self.channel_uses_aht = [false; MAX_CH];
        if ahte {
            for ch in usize::from(ncplblks != 6)..=self.channels {
                let mut use_aht = true;
                for blk in 1..6 {
                    if self.exp_strategy[blk][ch] != EXP_REUSE
                        || (ch == CPL && self.cpl_strategy_exists[blk])
                    {
                        use_aht = false;
                        break;
                    }
                }
                self.channel_uses_aht[ch] = use_aht && br.bit();
            }
        }

        // Frame level SNR offsets.
        if self.snr_offset_strategy == 0 {
            let csnr = (br.get(6) as i32 - 15) << 4;
            let snr = (csnr + br.get(4) as i32) << 2;
            self.snr_offset = [snr; MAX_CH];
        }

        // Transient pre-noise processing data.
        if transproce {
            for ch in 0..fbw {
                if br.bit() {
                    let loc = br.get(10) as u16;
                    let len = br.get(8) as u8;
                    self.pre_noise_data[ch] = Some((loc, len));
                }
            }
        }

        // Spectral extension attenuation.
        for ch in 1..=fbw {
            self.spx_atten_code[ch] = if spxattene && br.bit() { br.get(5) as i32 } else { -1 };
        }

        // Block start information.
        if nb > 1 && br.bit() {
            let words = hdr.frame_size / 2;
            let bits_per = 4 + (usize::BITS - (words.max(2) - 1).leading_zeros()) as usize;
            br.skip((nb - 1) * bits_per);
        }

        self.first_spx_coords = [true; MAX_CH];
        self.first_cpl_coords = [true; MAX_CH];
        self.first_cpl_leak = true;
        // Band structures return to their defaults on audio frame initialisation.
        self.spx_struct_valid = false;
        self.cpl_struct_valid = false;
        self.ecpl_struct_valid = false;

        bail!(br.overrun());
        Ok(())
    }

    /// Decodes a band structure for spectral extension or standard coupling.
    ///
    /// `bstruct` is indexed by absolute sub-band; returns the number of bands.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decode_band_structure(
        br: &mut BitReader<'_>,
        eac3: bool,
        start_subband: usize,
        end_subband: usize,
        defaults: &[u8],
        use_defaults: bool,
        bstruct: &mut [u8],
        sizes: &mut [u8],
    ) -> usize {
        if use_defaults {
            bstruct[..defaults.len()].copy_from_slice(defaults);
        }

        if !eac3 || br.bit() {
            for s in start_subband + 1..end_subband {
                bstruct[s] = br.get(1) as u8;
            }
        }

        let mut n = 1;
        sizes[0] = 12;
        for s in start_subband + 1..end_subband {
            if bstruct[s] != 0 {
                sizes[n - 1] += 12;
            }
            else {
                sizes[n] = 12;
                n += 1;
            }
        }
        n
    }

    fn coupling_strategy(&mut self, br: &mut BitReader<'_>) -> Res {
        let fbw = self.fbw;

        if !self.eac3 {
            self.cpl_in_use = br.bit();
        }

        if !self.cpl_in_use {
            for ch in 1..=fbw {
                self.channel_in_cpl[ch] = false;
                self.first_cpl_coords[ch] = true;
            }
            self.first_cpl_leak = self.eac3;
            self.phase_flags_in_use = false;
            self.ecpl_in_use = false;
            self.cpl_struct_valid = false;
            self.ecpl_struct_valid = false;
            return Ok(());
        }

        bail!(self.acmod < 2);

        self.ecpl_in_use = self.eac3 && br.bit();

        if self.eac3 && self.acmod == 2 {
            self.channel_in_cpl[1] = true;
            self.channel_in_cpl[2] = true;
        }
        else {
            for ch in 1..=fbw {
                self.channel_in_cpl[ch] = br.bit();
            }
        }

        if !self.ecpl_in_use {
            if self.acmod == 2 {
                self.phase_flags_in_use = br.bit();
            }

            let start = br.get(4) as i32;
            let end = if self.spx_in_use {
                (self.spx_src_start_freq as i32 - 37) / 12
            }
            else {
                br.get(4) as i32 + 3
            };
            bail!(start >= end);

            let (start, end) = (start as usize, end as usize);
            self.start_freq[CPL] = start * 12 + 37;
            self.end_freq[CPL] = end * 12 + 37;

            self.num_cpl_bands = Self::decode_band_structure(
                br,
                self.eac3,
                start,
                end,
                &DEFAULT_CPL_BAND_STRUCT,
                !self.cpl_struct_valid,
                &mut self.cpl_band_struct,
                &mut self.cpl_band_sizes,
            );
            self.cpl_struct_valid = true;
            self.ecpl_struct_valid = false;
        }
        else {
            self.phase_flags_in_use = false;
            self.ecpl_strategy(br)?;
            self.ecpl_struct_valid = true;
            self.cpl_struct_valid = false;
        }

        Ok(())
    }

    fn coupling_coordinates(&mut self, br: &mut BitReader<'_>, blk: usize) -> Res {
        if self.ecpl_in_use {
            return self.ecpl_coordinates(br);
        }

        let mut coords_exist = false;

        for ch in 1..=self.fbw {
            if self.channel_in_cpl[ch] {
                if (self.eac3 && self.first_cpl_coords[ch]) || br.bit() {
                    self.first_cpl_coords[ch] = false;
                    coords_exist = true;
                    let master = 3 * br.get(2);
                    for bnd in 0..self.num_cpl_bands {
                        let exp = br.get(4);
                        let mant = br.get(4);
                        let coord = if exp == 15 { mant << 22 } else { (mant + 16) << 21 };
                        self.cpl_coords[ch][bnd] = (coord >> (exp + master)) as i32;
                    }
                }
                else {
                    bail!(blk == 0);
                }
            }
            else {
                self.first_cpl_coords[ch] = true;
            }
        }

        if self.acmod == 2 && coords_exist {
            for bnd in 0..self.num_cpl_bands {
                self.phase_flags[bnd] = self.phase_flags_in_use && br.bit();
            }
        }

        Ok(())
    }

    /// Decodes the exponents of one channel.
    fn decode_exponents(
        br: &mut BitReader<'_>,
        strategy: u8,
        ngrps: usize,
        absexp: u8,
        dexps: &mut [u8],
    ) -> Res {
        // One exponent per bin for D15, per two bins for D25 and per four bins for D45.
        let group_size = usize::from(strategy) + usize::from(strategy == 3);

        bail!(dexps.len() < ngrps * 3 * group_size);

        let mut prev = i32::from(absexp);
        let mut j = 0;

        for _ in 0..ngrps {
            let code = br.get(7) as i32;
            bail!(code >= 125);

            for dexp in [code / 25, (code % 25) / 5, code % 5] {
                prev += dexp - 2;
                bail!(!(0..=24).contains(&prev));
                dexps[j..j + group_size].fill(prev as u8);
                j += group_size;
            }
        }
        Ok(())
    }

    /// Decodes one audio block, producing `BLOCK_SIZE` PCM samples per channel.
    fn decode_block(&mut self, br: &mut BitReader<'_>, blk: usize, lfg: &mut Lfg) -> Res {
        let fbw = self.fbw;
        let channels = self.channels;

        // Block switch flags.
        if self.block_switch_syntax {
            for ch in 1..=fbw {
                self.block_switch[ch] = br.bit();
            }
        }

        // Dither flags.
        if self.dither_flag_syntax {
            for ch in 1..=fbw {
                self.dither_flag[ch] = br.bit();
            }
        }

        // Dynamic range control.
        for i in 0..(if self.acmod == 0 { 2 } else { 1 }) {
            if br.bit() {
                self.dynamic_range[i] = DYNAMIC_RANGE[br.get(8) as usize];
            }
            else if blk == 0 {
                self.dynamic_range[i] = 1.0;
            }
        }

        // Spectral extension strategy.
        if self.eac3 && (blk == 0 || br.bit()) {
            self.spx_in_use = br.bit();
            if self.spx_in_use {
                self.spx_strategy(br)?;
            }
        }
        if !self.eac3 || !self.spx_in_use {
            self.spx_in_use = false;
            self.spx_struct_valid = false;
            self.channel_uses_spx = [false; MAX_CH];
            self.first_spx_coords = [true; MAX_CH];
        }

        // Spectral extension coordinates.
        if self.spx_in_use {
            self.spx_coordinates(br);
        }

        // Coupling strategy.
        if self.eac3 {
            self.cpl_in_use = self.cpl_in_use_blk[blk];
            if self.cpl_strategy_exists[blk] {
                self.coupling_strategy(br)?;
            }
        }
        else if br.bit() {
            // A new coupling strategy follows (starting with the coupling in use flag).
            self.coupling_strategy(br)?;
        }
        else {
            bail!(blk == 0);
        }

        let cpl_in_use = self.cpl_in_use;
        let first_ch = usize::from(!cpl_in_use);

        // Coupling coordinates.
        if cpl_in_use {
            self.coupling_coordinates(br, blk)?;
        }

        // Stereo rematrixing.
        if self.acmod == 2 {
            if (self.eac3 && blk == 0) || br.bit() {
                let cpl_bands = if !cpl_in_use {
                    None
                }
                else if self.ecpl_in_use {
                    Some(match self.ecpl_begf {
                        0 => 0,
                        1 => 1,
                        2 => 2,
                        3 | 4 => 3,
                        _ => 4,
                    })
                }
                else if self.start_freq[CPL] <= 61 {
                    Some(3 - usize::from(self.start_freq[CPL] == 37))
                }
                else {
                    None
                };
                self.num_rematrixing_bands =
                    cpl_bands.unwrap_or(if self.spx_in_use && self.spx_src_start_freq <= 61 {
                        3
                    }
                    else {
                        4
                    });
                for bnd in 0..self.num_rematrixing_bands {
                    self.rematrixing_flags[bnd] = br.bit();
                }
            }
            else if blk == 0 {
                self.num_rematrixing_bands = 0;
            }
        }

        // Exponent strategies.
        if !self.eac3 {
            for ch in first_ch..=channels {
                self.exp_strategy[blk][ch] =
                    br.get(if ch == self.lfe_ch && self.lfe_on { 1 } else { 2 }) as u8;
            }
        }

        // Channel bandwidth.
        for ch in 1..=fbw {
            self.start_freq[ch] = 0;
            let strategy = self.exp_strategy[blk][ch];
            if strategy != EXP_REUSE {
                self.end_freq[ch] = if self.channel_in_cpl[ch] {
                    self.start_freq[CPL]
                }
                else if self.channel_uses_spx[ch] {
                    self.spx_src_start_freq
                }
                else {
                    let bwcode = br.get(6) as usize;
                    bail!(bwcode > 60);
                    bwcode * 3 + 73
                };
                let group_size = 3usize << (strategy - 1);
                self.num_exp_groups[ch] =
                    (self.end_freq[ch] + group_size).saturating_sub(4) / group_size;
            }
        }
        if cpl_in_use && self.exp_strategy[blk][CPL] != EXP_REUSE {
            let group_size = 3usize << (self.exp_strategy[blk][CPL] - 1);
            self.num_exp_groups[CPL] =
                self.end_freq[CPL].saturating_sub(self.start_freq[CPL]) / group_size;
        }

        // Exponents.
        for ch in first_ch..=channels {
            let strategy = self.exp_strategy[blk][ch];
            if strategy != EXP_REUSE {
                let absexp = (br.get(4) << u32::from(ch == CPL)) as u8;
                self.dexps[ch][0] = absexp;
                let start = self.start_freq[ch] + usize::from(ch != CPL);
                bail!(start > 256);
                Self::decode_exponents(
                    br,
                    strategy,
                    self.num_exp_groups[ch],
                    absexp,
                    &mut self.dexps[ch][start..],
                )?;
                if ch != CPL && ch != self.lfe_ch {
                    br.skip(2); // gainrng
                }
            }
        }

        // Bit allocation parameters.
        if self.bit_allocation_syntax {
            if br.bit() {
                let shift = self.params.sr_shift;
                self.params.slow_decay = i32::from(SLOW_DECAY[br.get(2) as usize] >> shift);
                self.params.fast_decay = i32::from(FAST_DECAY[br.get(2) as usize] >> shift);
                self.params.slow_gain = i32::from(SLOW_GAIN[br.get(2) as usize]);
                self.params.db_per_bit = i32::from(DB_PER_BIT[br.get(2) as usize]);
                self.params.floor = i32::from(FLOOR[br.get(3) as usize]);
            }
            else {
                bail!(blk == 0);
            }
        }

        // SNR offsets and fast gains.
        if !self.eac3 {
            if br.bit() {
                let csnr = (br.get(6) as i32 - 15) << 4;
                for ch in first_ch..=channels {
                    self.snr_offset[ch] = (csnr + br.get(4) as i32) << 2;
                    self.fast_gain[ch] = i32::from(FAST_GAIN[br.get(3) as usize]);
                }
            }
            else {
                bail!(blk == 0);
            }
        }
        else if self.snr_offset_strategy != 0 && (blk == 0 || br.bit()) {
            let csnr = (br.get(6) as i32 - 15) << 4;
            if self.snr_offset_strategy == 1 {
                let snr = (csnr + br.get(4) as i32) << 2;
                self.snr_offset = [snr; MAX_CH];
            }
            else {
                for ch in first_ch..=channels {
                    self.snr_offset[ch] = (csnr + br.get(4) as i32) << 2;
                }
            }
        }

        // Fast gains (E-AC-3).
        if self.eac3 {
            if self.fast_gain_syntax && br.bit() {
                for ch in first_ch..=channels {
                    self.fast_gain[ch] = i32::from(FAST_GAIN[br.get(3) as usize]);
                }
            }
            else if blk == 0 {
                self.fast_gain = [i32::from(FAST_GAIN[4]); MAX_CH];
            }
        }

        // Converter SNR offset.
        if self.eac3 && self.is_independent && br.bit() {
            br.skip(10);
        }

        // Coupling leak.
        if cpl_in_use {
            if self.first_cpl_leak || br.bit() {
                self.params.cpl_fast_leak = br.get(3) as i32;
                self.params.cpl_slow_leak = br.get(3) as i32;
            }
            else {
                bail!(!self.eac3 && blk == 0);
            }
            self.first_cpl_leak = false;
        }

        // Delta bit allocation.
        if self.dba_syntax && br.bit() {
            for ch in first_ch..=fbw {
                self.dba_mode[ch] = match br.get(2) {
                    0 => DeltaMode::Reuse,
                    1 => DeltaMode::New,
                    2 => DeltaMode::None,
                    _ => return Err(Invalid),
                };
            }
            for ch in first_ch..=fbw {
                if self.dba_mode[ch] == DeltaMode::New {
                    let nsegs = br.get(3) as usize + 1;
                    self.dba_nsegs[ch] = nsegs;
                    for seg in 0..nsegs {
                        self.dba[ch][seg] = DeltaSeg {
                            offset: br.get(5) as u8,
                            len: br.get(4) as u8,
                            value: br.get(3) as u8,
                        };
                    }
                }
            }
        }
        else if blk == 0 {
            self.dba_mode = [DeltaMode::None; MAX_CH];
        }

        // Bit allocation.
        bail!(br.overrun());
        for ch in first_ch..=channels {
            let (start, end) = (self.start_freq[ch], self.end_freq[ch]);
            bail!(start >= end);

            bitalloc::calc_psd(
                &self.dexps[ch],
                start,
                end,
                &mut self.psd[ch],
                &mut self.band_psd[ch],
            );

            let nsegs = if self.dba_mode[ch] == DeltaMode::None { 0 } else { self.dba_nsegs[ch] };
            bail!(!bitalloc::calc_mask(
                &self.params,
                &self.band_psd[ch],
                start,
                end,
                self.fast_gain[ch],
                self.lfe_on && ch == self.lfe_ch,
                self.dba_mode[ch],
                &self.dba[ch][..nsegs],
                &mut self.mask[ch],
            ));

            let bap_tab = if self.channel_uses_aht[ch] { &HEBAP_TAB } else { &BAP_TAB };
            bitalloc::calc_bap(
                &self.mask[ch],
                &self.psd[ch],
                start,
                end,
                self.snr_offset[ch],
                self.params.floor,
                bap_tab,
                &mut self.bap[ch],
            );
        }

        // Unused dummy data.
        if self.skip_syntax && br.bit() {
            let skipl = br.get(9) as usize;
            br.skip(skipl * 8);
        }

        // Mantissas, including uncoupling.
        self.decode_transform_coeffs(br, blk, lfg)?;
        bail!(br.overrun());

        // Rematrixing.
        if self.acmod == 2 {
            self.do_rematrixing();
        }

        // Dynamic range scaling and conversion to floating point.
        for ch in 1..=channels {
            let audio_channel = usize::from(self.acmod == 0 && ch == 2);
            let gain = self.dynamic_range[audio_channel] * (1.0 / 4194304.0f32);
            for (t, &f) in self.tc[ch].iter_mut().zip(self.fixed[ch].iter()) {
                *t = f as f32 * gain;
            }
        }

        // Spectral extension.
        if self.spx_in_use {
            self.apply_spectral_extension(lfg);
        }

        // Inverse transform.
        let tr = transforms();
        for ch in 1..=channels {
            let short = ch <= fbw && self.block_switch[ch];
            let out = &mut self.pcm[ch - 1][blk * BLOCK_SIZE..(blk + 1) * BLOCK_SIZE];
            tr.block(&self.tc[ch], short, &mut self.delay[ch - 1], out);
        }

        Ok(())
    }

    /// Decodes the grouped and plain mantissas of one channel.
    fn decode_mantissas_ch(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        m: &mut MantGroups,
        lfg: &mut Lfg,
    ) {
        let start = self.start_freq[ch];
        let end = self.end_freq[ch].min(256);
        let dither = ch == CPL || self.dither_flag[ch];

        for freq in start..end {
            let bap = self.bap[ch][freq];
            let mantissa = match bap {
                0 => {
                    if dither {
                        // Random noise with an approximate range of -0.707 to 0.707.
                        (((lfg.get() >> 8) * 181) >> 8) as i32 - 5_931_008
                    }
                    else {
                        0
                    }
                }
                1 => {
                    if m.b1 > 0 {
                        m.b1 -= 1;
                        m.b1_mant[usize::from(m.b1)]
                    }
                    else {
                        let v = br.get(5) as i32;
                        m.b1_mant[1] = symmetric_dequant((v % 9) / 3, 3);
                        m.b1_mant[0] = symmetric_dequant(v % 3, 3);
                        m.b1 = 2;
                        symmetric_dequant(v / 9, 3)
                    }
                }
                2 => {
                    if m.b2 > 0 {
                        m.b2 -= 1;
                        m.b2_mant[usize::from(m.b2)]
                    }
                    else {
                        let v = br.get(7) as i32;
                        m.b2_mant[1] = symmetric_dequant((v % 25) / 5, 5);
                        m.b2_mant[0] = symmetric_dequant(v % 5, 5);
                        m.b2 = 2;
                        symmetric_dequant(v / 25, 5)
                    }
                }
                3 => symmetric_dequant(br.get(3) as i32, 7),
                4 => {
                    if m.b4 > 0 {
                        m.b4 = 0;
                        m.b4_mant
                    }
                    else {
                        let v = br.get(7) as i32;
                        m.b4_mant = symmetric_dequant(v % 11, 11);
                        m.b4 = 1;
                        symmetric_dequant(v / 11, 11)
                    }
                }
                5 => symmetric_dequant(br.get(4) as i32, 15),
                _ => {
                    // bap values above 15 are invalid in plain AC-3.
                    let bits = QUANTIZATION_BITS[usize::from(bap.min(15))];
                    br.get_signed(bits).wrapping_shl(24 - bits)
                }
            };
            self.fixed[ch][freq] = mantissa >> self.dexps[ch][freq];
        }
    }

    fn decode_transform_coeffs_ch(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        ch: usize,
        m: &mut MantGroups,
        lfg: &mut Lfg,
    ) -> Res {
        if !self.channel_uses_aht[ch] {
            self.decode_mantissas_ch(br, ch, m, lfg);
        }
        else {
            // The mantissas of all blocks are transmitted in the first block of the frame.
            if blk == 0 {
                self.decode_aht_ch(br, ch, lfg)?;
            }
            for bin in self.start_freq[ch]..self.end_freq[ch].min(256) {
                self.fixed[ch][bin] = self.pre_mant[ch][bin][blk] >> self.dexps[ch][bin];
            }
        }
        Ok(())
    }

    fn decode_transform_coeffs(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        lfg: &mut Lfg,
    ) -> Res {
        let mut m = MantGroups::default();
        let mut got_cplchan = false;

        for ch in 1..=self.channels {
            self.decode_transform_coeffs_ch(br, blk, ch, &mut m, lfg)?;

            let end = if self.channel_in_cpl[ch] && ch <= self.fbw {
                // The coupling channel follows the first coupled channel.
                if !got_cplchan {
                    self.decode_transform_coeffs_ch(br, blk, CPL, &mut m, lfg)?;
                    if self.ecpl_in_use {
                        self.calc_transform_coeffs_ecpl();
                    }
                    else {
                        self.calc_transform_coeffs_cpl();
                    }
                    got_cplchan = true;
                }
                self.end_freq[CPL]
            }
            else {
                self.end_freq[ch]
            };
            self.fixed[ch][end.min(256)..].fill(0);
        }

        // Remove the dither from coupled channels that do not use it.
        if self.cpl_in_use {
            for ch in 1..=self.fbw {
                if !self.dither_flag[ch] && self.channel_in_cpl[ch] {
                    for i in self.start_freq[CPL]..self.end_freq[CPL] {
                        if self.bap[CPL][i] == 0 {
                            self.fixed[ch][i] = 0;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Generates the coefficients of the coupled channels from the coupling channel.
    fn calc_transform_coeffs_cpl(&mut self) {
        let mut bin = self.start_freq[CPL];
        for band in 0..self.num_cpl_bands {
            let band_start = bin;
            let band_end = bin + usize::from(self.cpl_band_sizes[band]);
            for ch in 1..=self.fbw {
                if self.channel_in_cpl[ch] {
                    let coord = i64::from(self.cpl_coords[ch][band] << 5);
                    for b in band_start..band_end.min(256) {
                        let v = i64::from(self.fixed[CPL][b].wrapping_mul(1 << 4));
                        self.fixed[ch][b] = ((v * coord) >> 32) as i32;
                    }
                    if ch == 2 && self.phase_flags[band] {
                        for b in band_start..band_end.min(256) {
                            self.fixed[2][b] = self.fixed[2][b].wrapping_neg();
                        }
                    }
                }
            }
            bin = band_end;
        }
    }

    /// Stereo rematrixing of the coefficient bands that carry the sum and difference.
    fn do_rematrixing(&mut self) {
        let end = self.end_freq[1].min(self.end_freq[2]);
        for bnd in 0..self.num_rematrixing_bands {
            if self.rematrixing_flags[bnd] {
                let bnd_end = end.min(REMAT_BANDS[bnd + 1]);
                for i in REMAT_BANDS[bnd]..bnd_end {
                    let l = self.fixed[1][i];
                    let r = self.fixed[2][i];
                    self.fixed[1][i] = l.wrapping_add(r);
                    self.fixed[2][i] = l.wrapping_sub(r);
                }
            }
        }
    }
}

impl Default for Core {
    fn default() -> Self {
        Core::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Lfg;

    #[test]
    fn verify_lfg_seed_table() {
        // The first dither mantissas of FFmpeg's decoder (a lagged-Fibonacci generator seeded as
        // by `av_lfg_init`).
        let mut lfg = Lfg::new();
        let mut draw = || (((lfg.get() >> 8) * 181) >> 8) as i32 - 5_931_008;
        assert_eq!(draw(), 4_762_085);
        assert_eq!(draw(), 635_381);
        assert_eq!(draw(), 3_043_544);
    }
}
