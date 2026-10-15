// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dca_lbr.c, dcadsp.c (lbr_bank, lfe_iir) and dcahuff.c
// (LGPL-2.1+), see NOTICE.

//! DTS Express (DTS low bit rate, LBR) decoder.
//!
//! An LBR component lives in an extension substream asset. It is a chunked bitstream (least
//! significant bit first) that codes a 32-band (at 48 kHz) subband signal per channel with
//! interpolated scale factors, residual time samples that are quantized with a handful of
//! quantizers, and a set of tonal components. Reconstruction applies the scale factors, mid/side
//! and partial stereo, long term prediction of the lowest subbands, a 4-band hybrid filter bank
//! with a sinusoid synthesis and a windowed inverse MDCT with overlap-add. The low frequency
//! effects channel is coded as 64 ADPCM samples per frame and interpolated with an IIR filter.
//!
//! The bitstream syntax is not published in ETSI TS 102 114; this port follows the FFmpeg
//! decoder, which was written from the reference decoder's behaviour.

use std::sync::LazyLock;

use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::decoder::spkr;
use crate::exss::{ExssAsset, SAMPLING_FREQS};
use crate::tables_lbr::{
    AVG_G3_FREQS, BANK_COEFF, CORR_CF, FREQ_TO_SB, FST_AMP, GRID_1_TO_SCF, GRID_1_WEIGHTS,
    GRID_2_TO_SCF, LBR_VLC_SRC, LFE_DELTA_INDEX_16, LFE_DELTA_INDEX_24, LFE_IIR, LFE_STEP_SIZE_16,
    LFE_STEP_SIZE_24, LONG_WINDOW, PH0_SHIFT, QUANT_AMP, RSD_LEVEL_2A, RSD_LEVEL_2B, RSD_LEVEL_3,
    RSD_LEVEL_5, RSD_LEVEL_8, RSD_LEVEL_16, RSD_PACK_3_IN_7, RSD_PACK_5_IN_8, SB_REORDER,
    SCF_TO_GRID_1, SCF_TO_GRID_2, ST_COEFF, SYNTH_ENV,
};

const SYNC_LBR: u32 = 0x0a80_1921;

/// The maximum number of fullband channels decoded.
const CHANNELS: usize = 6;
/// The maximum number of fullband channels of a stream.
const CHANNELS_TOTAL: usize = 32;
/// The maximum number of subbands.
const SUBBANDS: usize = 32;
/// The size of the circular buffer of tones.
const TONES: usize = 512;
/// The number of time samples of a subband in a frame.
const TIME_SAMPLES: usize = 128;
/// The number of samples of history kept in front of the time samples of a subband.
const TIME_HISTORY: usize = 8;
/// The distance between the time samples of consecutive subbands in `time_samples`: the history,
/// the samples, and padding for the quantizers that write up to two samples past the end.
const TS_STRIDE: usize = TIME_SAMPLES + TIME_HISTORY * 2;
/// The largest scale factor index.
const AMP_MAX: u32 = 56;

/// Frequency range (number of doublings from 8 kHz of the number of subbands) of each sampling
/// frequency code. FFmpeg `dca.c`, `ff_dca_freq_ranges`.
const FREQ_RANGES: [u8; 16] = [0, 1, 2, 3, 4, 1, 2, 3, 4, 4, 0, 1, 2, 3, 4, 4];

const FLAG_LFE_PRESENT: u32 = 0x02;
const FLAG_BAND_LIMIT_1_2: u32 = 0x08;
const FLAG_BAND_LIMIT_1_4: u32 = 0x10;
const FLAG_BAND_LIMIT_NONE: u32 = 0x14;
const FLAG_BAND_LIMIT_MASK: u32 = 0x1c;
const FLAG_DMIX_STEREO: u32 = 0x20;
const FLAG_DMIX_MULTI_CH: u32 = 0x40;

const HEADER_SYNC_ONLY: u8 = 1;
const HEADER_DECODER_INIT: u8 = 2;

const CHUNK_FRAME: usize = 0x04;
const CHUNK_FRAME_NO_CSUM: usize = 0x06;
const CHUNK_LFE: usize = 0x0a;
const CHUNK_SCF: usize = 0x0e;
const CHUNK_TONAL: usize = 0x10;
const CHUNK_TONAL_GRP_1: usize = 0x11;
const CHUNK_TONAL_GRP_5: usize = 0x15;
const CHUNK_TONAL_SCF: usize = 0x16;
const CHUNK_TONAL_SCF_GRP_1: usize = 0x17;
const CHUNK_TONAL_SCF_GRP_5: usize = 0x1b;
const CHUNK_RES_GRID_LR: usize = 0x30;
const CHUNK_RES_GRID_HR: usize = 0x40;
const CHUNK_RES_TS_1: usize = 0x50;
const CHUNK_RES_TS_2: usize = 0x60;

/// The speaker pair of the speaker mask of the LBR stream that carries the low frequency effects
/// channel.
const SPEAKER_PAIR_LFE1: u32 = 0x0008;

/// `lpc_tab[i] = sin((i - 8) * (pi / (if i < 8 { 17 } else { 15 })))`, the reflection
/// coefficients of the subband predictors.
const LPC_TAB: [f32; 16] = [
    -0.995_734_2,
    -0.961_825_64,
    -0.895_163_3,
    -0.798_017_24,
    -0.673_695_6,
    -0.526_432_16,
    -0.361_241_67,
    -0.183_749_52,
    0.0,
    0.207_911_7,
    0.406_736_64,
    0.587_785_24,
    0.743_144_8,
    0.866_025_4,
    0.951_056_5,
    0.994_521_9,
];

/// Marker error of the chunk parsers. A chunk with an error is dropped, the frame is still
/// decoded from the other chunks.
#[derive(Debug)]
struct Invalid;

type Res = std::result::Result<(), Invalid>;

// -------------------------------------------------------------------------------------------
// Bit reader
// -------------------------------------------------------------------------------------------

/// A least significant bit first bit reader. Reads past the end of the buffer yield zero bits
/// and the position keeps advancing, [`LeBits::left`] then returns a negative number.
struct LeBits<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> LeBits<'a> {
    fn new(buf: &'a [u8]) -> Self {
        LeBits { buf, pos: 0 }
    }

    /// Peek up to 32 bits.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;

        let mut v = 0u64;
        for i in 0..5 {
            v |= u64::from(self.buf.get(byte + i).copied().unwrap_or(0)) << (8 * i);
        }

        ((v >> shift) & ((1u64 << n) - 1)) as u32
    }

    #[inline]
    fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    #[inline]
    fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    /// Read `n` bits, `n` may be 0.
    #[inline]
    fn get_bitsz(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.get(n) }
    }

    /// The number of bits left, negative if the reader has read past the end.
    #[inline]
    fn left(&self) -> i64 {
        (self.buf.len() * 8) as i64 - self.pos as i64
    }

    /// Check that at least `n` bits are left. Returns `true`, after skipping the remaining bits,
    /// if not: the caller must stop parsing the chunk. Truncated data is expected (the encoder
    /// drops the least important data first) and is not an error.
    fn ensure(&mut self, n: i64) -> bool {
        let left = self.left();
        if left < 0 {
            return true;
        }
        if left < n {
            self.pos = self.buf.len() * 8;
            return true;
        }
        false
    }
}

/// A byte reader whose reads past the end return zero.
struct Bytes<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Bytes { buf, pos: 0 }
    }

    fn left(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn u8(&mut self) -> u8 {
        let v = self.buf.get(self.pos).copied().unwrap_or(0);
        self.pos = (self.pos + 1).min(self.buf.len());
        v
    }

    fn u16_le(&mut self) -> u32 {
        let lo = u32::from(self.u8());
        lo | u32::from(self.u8()) << 8
    }

    fn u16_be(&mut self) -> u32 {
        let hi = u32::from(self.u8());
        hi << 8 | u32::from(self.u8())
    }

    fn u32_be(&mut self) -> u32 {
        let hi = self.u16_be();
        hi << 16 | self.u16_be()
    }

    /// Take `n` bytes.
    fn take(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.left());
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        s
    }
}

// -------------------------------------------------------------------------------------------
// Huffman code books
// -------------------------------------------------------------------------------------------

/// A Huffman code book of the LBR bitstream.
///
/// The source lists `(symbol, length)` pairs; codes are assigned sequentially in table order,
/// MSB aligned, like the core code books. The first bit of a code is the first bit read from
/// the (least significant bit first) stream, so a lookup table indexed by the peeked bits
/// is built from the bit-reversed codes. Code books may be incomplete.
struct LbrVlc {
    /// `(value, length)` for every combination of `bits` peeked bits, length 0 if no code
    /// matches.
    lut: Vec<(i16, u8)>,
    bits: u32,
}

impl LbrVlc {
    fn new(src: &[(u8, u8)], offset: i32) -> Self {
        let bits = src.iter().map(|&(_, l)| u32::from(l)).max().unwrap_or(1);
        let mut lut = vec![(0i16, 0u8); 1 << bits];

        let mut code = 0u64;
        for &(sym, len) in src {
            let len = u32::from(len);
            let value = (i32::from(sym) + offset) as i16;

            // Sequential codes of a valid book never overflow.
            let rev = ((code >> (32 - len)) as u32).reverse_bits() >> (32 - len);
            for hi in 0..1usize << (bits - len) {
                lut[rev as usize | (hi << len)] = (value, len as u8);
            }

            code += 1u64 << (32 - len);
        }

        debug_assert!(code <= 1u64 << 32);

        LbrVlc { lut, bits }
    }

    /// Decode a symbol. Returns -1 for the escape symbol and for bit patterns without a code.
    #[inline]
    fn decode(&self, gb: &mut LeBits<'_>) -> i32 {
        let (value, len) = self.lut[gb.peek(self.bits) as usize];
        gb.pos += usize::from(len);
        i32::from(value)
    }

    /// Decode a symbol, or the explicit value that follows the escape symbol.
    #[inline]
    fn parse(&self, gb: &mut LeBits<'_>) -> i32 {
        let v = self.decode(gb);
        if v >= 0 {
            return v;
        }
        // Rare value.
        let n = gb.get(3) + 1;
        gb.get(n) as i32
    }
}

struct Huffman {
    tnl_grp: [LbrVlc; 5],
    tnl_scf: LbrVlc,
    damp: LbrVlc,
    dph: LbrVlc,
    fst_rsd_amp: LbrVlc,
    rsd_apprx: LbrVlc,
    rsd_amp: LbrVlc,
    avg_g3: LbrVlc,
    st_grid: LbrVlc,
    grid_2: LbrVlc,
    grid_3: LbrVlc,
    rsd: LbrVlc,
}

impl Huffman {
    fn get() -> &'static Huffman {
        static TABLES: LazyLock<Huffman> = LazyLock::new(Huffman::build);
        &TABLES
    }

    fn build() -> Huffman {
        const TNL_GRP_SIZES: [usize; 5] = [37, 34, 31, 28, 23];

        let mut pos = 0;
        let mut take = |count: usize, offset: i32| {
            let t = LbrVlc::new(&LBR_VLC_SRC[pos..pos + count], offset);
            pos += count;
            t
        };

        Huffman {
            tnl_grp: TNL_GRP_SIZES.map(|n| take(n, -1)),
            tnl_scf: take(20, -1),
            damp: take(7, -1),
            dph: take(9, -1),
            fst_rsd_amp: take(24, -1),
            rsd_apprx: take(6, -1),
            rsd_amp: take(33, -1),
            avg_g3: take(18, -1),
            st_grid: take(22, -1),
            grid_2: take(20, -1),
            grid_3: take(13, -1),
            rsd: take(9, 0),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Decoder
// -------------------------------------------------------------------------------------------

#[derive(Copy, Clone, Default)]
struct Tone {
    /// Spectral line offset.
    x_freq: u8,
    /// Difference between the original and the center frequency.
    f_delt: u8,
    /// Phase rotation.
    ph_rot: u8,
    /// Per-channel amplitude.
    amp: [u8; CHANNELS],
    /// Per-channel phase.
    phs: [u8; CHANNELS],
}

#[derive(Copy, Clone, Default)]
struct Chunk<'a> {
    id: usize,
    data: &'a [u8],
}

/// Information about a decoded frame.
pub struct LbrFrame {
    pub nsamples: usize,
    pub sample_rate: u32,
    /// The DTS speakers of the output channels, the samples are in [`LbrDecoder::out`].
    pub ch_mask: u32,
}

pub struct LbrDecoder {
    sample_rate: u32,
    ch_mask: u32,
    flags: u32,
    bit_rate_orig: u32,
    bit_rate_scaled: u32,

    /// Number of fullband channels to decode.
    nchannels: usize,
    /// Total number of fullband channels.
    nchannels_total: usize,
    freq_range: usize,
    band_limit: usize,
    limited_rate: u32,
    limited_range: usize,
    res_profile: usize,
    /// Number of encoded subbands.
    nsubbands: usize,
    /// Subband index where grid 3 scale factors end.
    g3_avg_only_start_sb: usize,
    /// Subband index where mono encoding starts.
    min_mono_subband: usize,
    /// Subband index where mono encoding ends.
    max_mono_subband: usize,

    /// Lower 5 bits of the current frame number.
    framenum: usize,
    /// Seed for subband randomization.
    lbr_rand: i32,

    quant_levels: [[u8; SUBBANDS]; CHANNELS / 2],
    /// Subband reordering indices.
    sb_indices: [u8; SUBBANDS],
    /// Right channel inversion or mid/side decoding flags.
    sec_ch_sbms: [[u8; SUBBANDS]; CHANNELS / 2],
    /// Flags indicating if left/right channels are swapped.
    sec_ch_lrms: [[u8; SUBBANDS]; CHANNELS / 2],
    /// Subband allocation flags.
    ch_pres: [u32; CHANNELS],

    grid_1_scf: [[[u8; 8]; 12]; CHANNELS],
    grid_2_scf: [[[u8; 64]; 3]; CHANNELS],
    grid_3_avg: [[i8; SUBBANDS - 4]; CHANNELS],
    grid_3_scf: [[[i8; 8]; SUBBANDS - 4]; CHANNELS],
    grid_3_pres: [u32; CHANNELS],
    high_res_scf: [[[u8; 8]; SUBBANDS]; CHANNELS],

    part_stereo: [[[u8; 5]; SUBBANDS / 4]; CHANNELS],
    part_stereo_pres: u8,

    /// Predictor coefficients: frame parity, channel, subband, set.
    lpc_coeff: [[[[[f32; 8]; 2]; 3]; CHANNELS]; 2],

    /// Subband randomization scale factors.
    sb_scf: [f32; SUBBANDS],

    /// Time samples, `TS_STRIDE` per subband with `TIME_HISTORY` samples of history in front.
    time_samples: Vec<f32>,

    /// IMDCT overlap history.
    history: [[f32; SUBBANDS * 4]; CHANNELS],
    /// Long window of the IMDCT.
    window: Vec<f32>,
    /// DCT-IV matrix of the IMDCT, scaled.
    dct: Vec<f32>,

    /// Decimated LFE samples.
    lfe_data: [f32; 64],
    /// LFE IIR filter history.
    lfe_history: [[f32; 2]; 5],
    /// Scale factor of the LFE samples before the IIR filter.
    lfe_scale: f32,

    tonal_scf: [u8; 6],
    /// Per-group per-subframe start and end position of tones.
    tonal_bounds: [[[u16; 2]; 32]; 5],
    /// Circular buffer of tones.
    tones: Vec<Tone>,
    /// Circular buffer head position.
    ntones: usize,

    /// Output samples indexed by DTS speaker.
    pub out: Vec<Vec<f32>>,
}

fn ceil_log2(x: u32) -> u32 {
    if x > 1 { 32 - (x - 1).leading_zeros() } else { 0 }
}

/// The number of channels of a speaker pair mask.
fn count_chs_for_mask(mask: u32) -> usize {
    (((mask & 0xffff) | ((mask & 0xae66) << 16)).count_ones()) as usize
}

impl Default for LbrDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LbrDecoder {
    pub fn new() -> Self {
        LbrDecoder {
            sample_rate: 0,
            ch_mask: 0,
            flags: 0,
            bit_rate_orig: 0,
            bit_rate_scaled: 0,
            nchannels: 0,
            nchannels_total: 0,
            freq_range: 0,
            band_limit: 0,
            limited_rate: 0,
            limited_range: 0,
            res_profile: 0,
            nsubbands: 0,
            g3_avg_only_start_sb: 0,
            min_mono_subband: 0,
            max_mono_subband: 0,
            framenum: 0,
            lbr_rand: 1,
            quant_levels: [[0; SUBBANDS]; CHANNELS / 2],
            sb_indices: [0xff; SUBBANDS],
            sec_ch_sbms: [[0; SUBBANDS]; CHANNELS / 2],
            sec_ch_lrms: [[0; SUBBANDS]; CHANNELS / 2],
            ch_pres: [0; CHANNELS],
            grid_1_scf: [[[0; 8]; 12]; CHANNELS],
            grid_2_scf: [[[0; 64]; 3]; CHANNELS],
            grid_3_avg: [[0; SUBBANDS - 4]; CHANNELS],
            grid_3_scf: [[[0; 8]; SUBBANDS - 4]; CHANNELS],
            grid_3_pres: [0; CHANNELS],
            high_res_scf: [[[0; 8]; SUBBANDS]; CHANNELS],
            part_stereo: [[[16; 5]; SUBBANDS / 4]; CHANNELS],
            part_stereo_pres: 0,
            lpc_coeff: [[[[[0.0; 8]; 2]; 3]; CHANNELS]; 2],
            sb_scf: [0.0; SUBBANDS],
            time_samples: vec![0.0; CHANNELS * SUBBANDS * TS_STRIDE],
            history: [[0.0; SUBBANDS * 4]; CHANNELS],
            window: Vec::new(),
            dct: Vec::new(),
            lfe_data: [0.0; 64],
            lfe_history: [[0.0; 2]; 5],
            lfe_scale: 0.0,
            tonal_scf: [0; 6],
            tonal_bounds: [[[0; 2]; 32]; 5],
            tones: vec![Tone::default(); TONES],
            ntones: 0,
            out: vec![Vec::new(); spkr::COUNT],
        }
    }

    /// Clear all history.
    pub fn flush(&mut self) {
        if self.sample_rate == 0 {
            return;
        }

        self.part_stereo = [[[16; 5]; SUBBANDS / 4]; CHANNELS];
        self.lpc_coeff = [[[[[0.0; 8]; 2]; 3]; CHANNELS]; 2];
        self.history = [[0.0; SUBBANDS * 4]; CHANNELS];
        self.tonal_bounds = [[[0; 2]; 32]; 5];
        self.lfe_history = [[0.0; 2]; 5];
        self.framenum = 0;
        self.ntones = 0;

        for ch in 0..self.nchannels {
            for sb in 0..self.nsubbands {
                let base = Self::ts_base(ch, sb);
                self.time_samples[base - TIME_HISTORY..base].fill(0.0);
            }
        }
    }

    /// The index of the first time sample of a subband.
    #[inline]
    fn ts_base(ch: usize, sb: usize) -> usize {
        (ch * SUBBANDS + sb) * TS_STRIDE + TIME_HISTORY
    }

    fn lbr_rand(&mut self, sb: usize) -> f32 {
        self.lbr_rand = 1103515245u32.wrapping_mul(self.lbr_rand as u32).wrapping_add(12345) as i32;
        self.lbr_rand as f32 * self.sb_scf[sb]
    }

    // ---------------------------------------------------------------------------------------
    // Initialization
    // ---------------------------------------------------------------------------------------

    fn init_sample_rate(&mut self) {
        // The inverse MDCT of the filter bank has N = 32 << freq_range coefficients, the scale
        // is the normalization of the reference (its sign is absorbed by the unfolding below).
        let n = 32usize << self.freq_range;
        let scale =
            (1.0 / f64::from(1u32 << 17)) * f64::from(1u32 << (2 - self.limited_range)).sqrt();
        let scale = f64::from(scale as f32);

        // DCT-IV: C[j] = sum_k X[k] cos(pi / N * (j + 1/2) * (k + 1/2)).
        self.dct = vec![0.0; n * n];
        for j in 0..n {
            for k in 0..n {
                let arg = std::f64::consts::PI / n as f64 * (j as f64 + 0.5) * (k as f64 + 0.5);
                self.dct[j * n + k] = (scale * arg.cos()) as f32;
            }
        }

        self.window = (0..n).map(|i| LONG_WINDOW[i << (2 - self.freq_range)]).collect();

        let br_per_ch = f64::from(self.bit_rate_scaled / self.nchannels_total as u32);
        let mut scale = if br_per_ch < 14000.0 {
            0.85
        }
        else if br_per_ch < 32000.0 {
            (br_per_ch - 14000.0) * (1.0 / 120000.0) + 0.85
        }
        else {
            1.0
        };

        scale *= 1.0 / f64::from(i32::MAX);

        for i in 0..self.nsubbands {
            self.sb_scf[i] = if i < 2 {
                // The first two subbands are always zero.
                0.0
            }
            else if i < 5 {
                ((i - 1) as f64 * 0.25 * 0.785 * scale) as f32
            }
            else {
                (0.785 * scale) as f32
            };
        }

        self.lfe_scale = ((16u32 << self.freq_range) as f64 * 0.0000078265894) as f32;
    }

    fn parse_decoder_init(&mut self, gb: &mut Bytes<'_>) -> Result<()> {
        let old_rate = self.sample_rate;
        let old_band_limit = self.band_limit;
        let old_nchannels = self.nchannels;

        // Sample rate of the LBR audio.
        let sr_code = usize::from(gb.u8());
        if sr_code >= SAMPLING_FREQS.len() {
            return decode_error("dca: invalid lbr sample rate");
        }
        self.sample_rate = SAMPLING_FREQS[sr_code];
        if self.sample_rate > 48000 {
            return unsupported_error("dca: lbr sample rates above 48 kHz are not supported");
        }

        // LBR speaker mask.
        self.ch_mask = gb.u16_le();
        if self.ch_mask & 0x7 == 0 {
            return unsupported_error("dca: unsupported lbr channel mask");
        }
        if self.ch_mask & 0xfff0 != 0 {
            log::warn!(
                "dca: lbr channel mask {:#x} has channels that are not decoded",
                self.ch_mask
            );
        }

        // LBR bitstream version.
        let version = gb.u16_le();
        if version & 0xff00 != 0x0800 {
            return unsupported_error("dca: unsupported lbr stream version");
        }

        // Flags for LBR decoder initialization.
        self.flags = u32::from(gb.u8());
        if self.flags & FLAG_DMIX_MULTI_CH != 0 {
            return unsupported_error("dca: lbr multi-channel downmix is not supported");
        }
        if self.flags & FLAG_LFE_PRESENT != 0 && self.sample_rate != 48000 {
            // The LFE channel is not interpolated for other sample rates.
            self.flags &= !FLAG_LFE_PRESENT;
        }

        // Most significant bit rate nibbles.
        let bit_rate_hi = u32::from(gb.u8());

        // Original bit rate.
        self.bit_rate_orig = gb.u16_le() | ((bit_rate_hi & 0x0f) << 16);

        // Scaled bit rate.
        self.bit_rate_scaled = gb.u16_le() | ((bit_rate_hi & 0xf0) << 12);

        // Number of fullband channels.
        self.nchannels_total = count_chs_for_mask(self.ch_mask & !SPEAKER_PAIR_LFE1);
        self.nchannels = self.nchannels_total.min(CHANNELS);

        // Band limit.
        self.band_limit = match self.flags & FLAG_BAND_LIMIT_MASK {
            FLAG_BAND_LIMIT_NONE => 0,
            FLAG_BAND_LIMIT_1_2 => 1,
            FLAG_BAND_LIMIT_1_4 => 2,
            _ => return unsupported_error("dca: unsupported lbr band limit"),
        };

        // Frequency range.
        self.freq_range = usize::from(FREQ_RANGES[sr_code]);

        // Resolution profile.
        let ntotal = self.nchannels_total as u32 + 2;
        self.res_profile = if self.bit_rate_orig >= 44000 * ntotal {
            2
        }
        else if self.bit_rate_orig >= 25000 * ntotal {
            1
        }
        else {
            0
        };

        // Limited sample rate, number of subbands, etc.
        self.limited_rate = self.sample_rate >> self.band_limit;
        if self.band_limit > self.freq_range {
            return decode_error("dca: invalid lbr band limit for the frequency range");
        }
        self.limited_range = self.freq_range - self.band_limit;

        self.nsubbands = 8 << self.limited_range;

        let half_rate = (self.limited_rate / 2) as usize;
        self.g3_avg_only_start_sb = (self.nsubbands * usize::from(AVG_G3_FREQS[self.res_profile])
            / half_rate)
            .min(self.nsubbands);
        self.min_mono_subband = (self.nsubbands * 2000 / half_rate).min(self.nsubbands);
        self.max_mono_subband = (self.nsubbands * 14000 / half_rate).min(self.nsubbands);

        // Handle a change of sample rate.
        if old_rate != self.sample_rate || old_band_limit != self.band_limit {
            self.init_sample_rate();
        }

        // Stereo downmix.
        if self.flags & FLAG_DMIX_STEREO != 0 {
            if self.nchannels_total < 3 || self.nchannels_total > CHANNELS_TOTAL - 2 {
                return decode_error("dca: invalid number of channels for the lbr stereo downmix");
            }

            // The embedded downmix (ECS chunk) is not used: account for the downmixed channel
            // pair, which is decoded as the stream.
            self.nchannels_total += 2;
            self.nchannels = 2;
            self.ch_mask = 0x0002;
            self.flags &= !FLAG_LFE_PRESENT;
        }

        // Handle a change of sample rate or number of channels.
        if old_rate != self.sample_rate
            || old_band_limit != self.band_limit
            || old_nchannels != self.nchannels
        {
            self.time_samples.fill(0.0);
            self.flush();
        }

        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Frame parsing
    // ---------------------------------------------------------------------------------------

    /// Parse an LBR component of an extension substream asset. `data` starts at the sync word of
    /// the extension substream.
    pub fn parse(&mut self, data: &[u8], asset: &ExssAsset) -> Result<()> {
        let Some(comp) =
            data.get(asset.lbr_offset..asset.lbr_offset.saturating_add(asset.lbr_size))
        else {
            return decode_error("dca: lbr component outside of the packet");
        };

        let mut gb = Bytes::new(comp);

        // LBR sync word.
        if gb.u32_be() != SYNC_LBR {
            return decode_error("dca: invalid lbr sync word");
        }

        // LBR header type.
        match gb.u8() {
            HEADER_SYNC_ONLY => {
                if self.sample_rate == 0 {
                    return decode_error("dca: lbr decoder is not initialized");
                }
            }
            HEADER_DECODER_INIT => {
                if let Err(e) = self.parse_decoder_init(&mut gb) {
                    self.sample_rate = 0;
                    return Err(e);
                }
            }
            _ => return decode_error("dca: invalid lbr header type"),
        }

        // LBR frame chunk header.
        let chunk_id = usize::from(gb.u8());
        let mut chunk_len =
            if chunk_id & 0x80 != 0 { gb.u16_be() as usize } else { usize::from(gb.u8()) };

        if chunk_len > gb.left() {
            chunk_len = gb.left();
            log::warn!("dca: lbr frame chunk was truncated");
        }

        let mut gb = Bytes::new(gb.take(chunk_len));

        match chunk_id & 0x7f {
            CHUNK_FRAME => {
                // Checksum.
                gb.take(2);
            }
            CHUNK_FRAME_NO_CSUM => (),
            _ => return decode_error("dca: invalid lbr frame chunk id"),
        }

        // Clear the current frame.
        self.quant_levels = [[0; SUBBANDS]; CHANNELS / 2];
        self.sb_indices = [0xff; SUBBANDS];
        self.sec_ch_sbms = [[0; SUBBANDS]; CHANNELS / 2];
        self.sec_ch_lrms = [[0; SUBBANDS]; CHANNELS / 2];
        self.ch_pres = [0; CHANNELS];
        self.grid_1_scf = [[[0; 8]; 12]; CHANNELS];
        self.grid_2_scf = [[[0; 64]; 3]; CHANNELS];
        self.grid_3_avg = [[0; SUBBANDS - 4]; CHANNELS];
        self.grid_3_scf = [[[0; 8]; SUBBANDS - 4]; CHANNELS];
        self.grid_3_pres = [0; CHANNELS];
        self.tonal_scf = [0; 6];
        self.lfe_data = [0.0; 64];
        self.part_stereo_pres = 0;
        self.framenum = (self.framenum + 1) & 31;

        for ch in 0..self.nchannels {
            for sb in 0..self.nsubbands / 4 {
                self.part_stereo[ch][sb][0] = self.part_stereo[ch][sb][4];
                self.part_stereo[ch][sb][4] = 16;
            }
        }

        self.lpc_coeff[self.framenum & 1] = [[[[0.0; 8]; 2]; 3]; CHANNELS];

        for group in 0..5 {
            for sf in 0..1usize << group {
                let sf_idx = ((self.framenum << group) + sf) & 31;
                self.tonal_bounds[group][sf_idx] = [self.ntones as u16; 2];
            }
        }

        // Parse the chunk headers.
        let mut lfe = Chunk::default();
        let mut tonal = Chunk::default();
        let mut tonal_grp = [Chunk::default(); 5];
        let mut grid1 = [Chunk::default(); CHANNELS / 2];
        let mut hr_grid = [Chunk::default(); CHANNELS / 2];
        let mut ts1 = [Chunk::default(); CHANNELS / 2];
        let mut ts2 = [Chunk::default(); CHANNELS / 2];

        while gb.left() > 0 {
            let id = usize::from(gb.u8());
            let mut len = if id & 0x80 != 0 { gb.u16_be() as usize } else { usize::from(gb.u8()) };
            let id = id & 0x7f;

            if len > gb.left() {
                len = gb.left();
                log::warn!("dca: lbr chunk {:#x} was truncated", id);
            }

            let data = gb.take(len);

            match id {
                CHUNK_LFE => lfe = Chunk { id, data },
                CHUNK_SCF | CHUNK_TONAL | CHUNK_TONAL_SCF => tonal = Chunk { id, data },
                CHUNK_TONAL_GRP_1..=CHUNK_TONAL_GRP_5 => {
                    let i = CHUNK_TONAL_GRP_5 - id;
                    tonal_grp[i] = Chunk { id: i, data };
                }
                CHUNK_TONAL_SCF_GRP_1..=CHUNK_TONAL_SCF_GRP_5 => {
                    let i = CHUNK_TONAL_SCF_GRP_5 - id;
                    tonal_grp[i] = Chunk { id: i, data };
                }
                CHUNK_RES_GRID_LR..=0x32 => grid1[id - CHUNK_RES_GRID_LR] = Chunk { id, data },
                CHUNK_RES_GRID_HR..=0x42 => hr_grid[id - CHUNK_RES_GRID_HR] = Chunk { id, data },
                CHUNK_RES_TS_1..=0x52 => ts1[id - CHUNK_RES_TS_1] = Chunk { id, data },
                CHUNK_RES_TS_2..=0x62 => ts2[id - CHUNK_RES_TS_2] = Chunk { id, data },
                _ => (),
            }
        }

        // Parse the chunks. A damaged chunk does not fail the frame.
        let _ = self.parse_lfe_chunk(&lfe);
        let _ = self.parse_tonal_chunk(&tonal);

        for chunk in &tonal_grp {
            let _ = self.parse_tonal_group(chunk);
        }

        for i in 0..self.nchannels.div_ceil(2) {
            let ch1 = i * 2;
            let ch2 = (ch1 + 1).min(self.nchannels - 1);

            if self.parse_grid_1_chunk(&grid1[i], ch1, ch2).is_err()
                || self.parse_high_res_grid(&hr_grid[i], ch1, ch2).is_err()
            {
                continue;
            }

            // The time sample chunks depend on both grids. TS_2 depends on TS_1.
            if grid1[i].data.is_empty() || hr_grid[i].data.is_empty() || ts1[i].data.is_empty() {
                continue;
            }

            if self.parse_ts1_chunk(&ts1[i], ch1, ch2).is_err() {
                continue;
            }

            let _ = self.parse_ts2_chunk(&ts2[i], ch1, ch2);
        }

        Ok(())
    }

    fn parse_lfe_24(&mut self, gb: &mut LeBits<'_>) -> Res {
        let step_max = LFE_STEP_SIZE_24.len() as i32 - 1;

        let ps = gb.get(24) as i32;
        let si = ps >> 23;
        let mut value = ((((ps & 0x7fffff) ^ -si) + si) as f32) * (1.0 / 0x7fffff as f32);

        let mut step_i = gb.get(8) as i32;
        if step_i > step_max {
            return Err(Invalid);
        }

        let mut step = LFE_STEP_SIZE_24[step_i as usize];

        for i in 0..64 {
            let code = gb.get(6) as usize;

            let mut delta = step * 0.03125;
            if code & 16 != 0 {
                delta += step;
            }
            if code & 8 != 0 {
                delta += step * 0.5;
            }
            if code & 4 != 0 {
                delta += step * 0.25;
            }
            if code & 2 != 0 {
                delta += step * 0.125;
            }
            if code & 1 != 0 {
                delta += step * 0.0625;
            }

            if code & 32 != 0 {
                value -= delta;
                value = value.max(-3.0);
            }
            else {
                value += delta;
                value = value.min(3.0);
            }

            step_i += i32::from(LFE_DELTA_INDEX_24[code & 31]);
            step_i = step_i.clamp(0, step_max);

            step = LFE_STEP_SIZE_24[step_i as usize];
            self.lfe_data[i] = value * self.lfe_scale;
        }

        Ok(())
    }

    fn parse_lfe_16(&mut self, gb: &mut LeBits<'_>) -> Res {
        let step_max = LFE_STEP_SIZE_16.len() as i32 - 1;

        let ps = gb.get(16) as i32;
        let si = ps >> 15;
        let mut value = ((((ps & 0x7fff) ^ -si) + si) as f32) * (1.0 / 0x7fff as f32);

        let mut step_i = gb.get(8) as i32;
        if step_i > step_max {
            return Err(Invalid);
        }

        let mut step = LFE_STEP_SIZE_16[step_i as usize];

        for i in 0..64 {
            let code = gb.get(4) as usize;

            let mut delta = step * 0.125;
            if code & 4 != 0 {
                delta += step;
            }
            if code & 2 != 0 {
                delta += step * 0.5;
            }
            if code & 1 != 0 {
                delta += step * 0.25;
            }

            if code & 8 != 0 {
                value -= delta;
                value = value.max(-3.0);
            }
            else {
                value += delta;
                value = value.min(3.0);
            }

            step_i += i32::from(LFE_DELTA_INDEX_16[code & 7]);
            step_i = step_i.clamp(0, step_max);

            step = LFE_STEP_SIZE_16[step_i as usize];
            self.lfe_data[i] = value * self.lfe_scale;
        }

        Ok(())
    }

    fn parse_lfe_chunk(&mut self, chunk: &Chunk<'_>) -> Res {
        if self.flags & FLAG_LFE_PRESENT == 0 || chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);

        // The bit depth follows from the size of the chunk.
        if chunk.data.len() >= 52 {
            self.parse_lfe_24(&mut gb)
        }
        else if chunk.data.len() >= 35 {
            self.parse_lfe_16(&mut gb)
        }
        else {
            Err(Invalid)
        }
    }

    fn parse_tonal(&mut self, gb: &mut LeBits<'_>, group: usize) -> Res {
        let huff = Huffman::get();

        let mut amp = [0u32; CHANNELS_TOTAL];
        let mut phs = [0u32; CHANNELS_TOTAL];
        let ch_nbits = ceil_log2(self.nchannels_total as u32);

        // Parse the subframes of this group.
        let mut sf = 0usize;
        while sf < 1 << group {
            let sf_idx = ((self.framenum << group) + sf) & 31;
            self.tonal_bounds[group][sf_idx][0] = self.ntones as u16;

            // Parse the tones of this subframe.
            let mut freq = 1u32;
            let diff = loop {
                if gb.left() < 1 {
                    return Err(Invalid);
                }

                let idx = huff.tnl_grp[group].parse(gb) as usize;
                if idx >= FST_AMP.len() {
                    return Err(Invalid);
                }

                let diff = gb.get_bitsz((idx >> 2) as u32) + u32::from(FST_AMP[idx]);
                if diff <= 1 {
                    // End of the subframe.
                    break diff;
                }

                freq += diff - 2;
                if (freq >> (5 - group)) as usize > self.nsubbands * 4 - 6 {
                    return Err(Invalid);
                }

                // Main channel.
                let main_ch = gb.get_bitsz(ch_nbits) as usize;
                let main_amp = (huff.tnl_scf.parse(gb) as u32)
                    .wrapping_add(u32::from(
                        self.tonal_scf[usize::from(FREQ_TO_SB[(freq >> (7 - group)) as usize])],
                    ))
                    .wrapping_add(self.limited_range as u32)
                    .wrapping_sub(2);
                amp[main_ch] = if main_amp < AMP_MAX { main_amp } else { 0 };
                phs[main_ch] = gb.get(3);

                // Secondary channels.
                for ch in 0..self.nchannels_total {
                    if ch == main_ch {
                        continue;
                    }
                    if gb.get1() {
                        amp[ch] = amp[main_ch].wrapping_sub(huff.damp.parse(gb) as u32);
                        phs[ch] = phs[main_ch].wrapping_sub(huff.dph.parse(gb) as u32);
                    }
                    else {
                        amp[ch] = 0;
                        phs[ch] = 0;
                    }
                }

                if amp[main_ch] != 0 {
                    // Allocate a new tone.
                    let x_freq = (freq >> (5 - group)) as u8;
                    let f_delt = (((freq & ((1 << (5 - group)) - 1)) << group) & 0xff) as u8;
                    let ph_rot =
                        (256i32 - i32::from(x_freq & 1) * 128 - i32::from(f_delt) * 4) as u8;

                    let shift =
                        i32::from(PH0_SHIFT[usize::from(x_freq & 3) * 2 + (freq & 1) as usize])
                            - ((i32::from(ph_rot) << (5 - group)) - i32::from(ph_rot));

                    let t = &mut self.tones[self.ntones];
                    self.ntones = (self.ntones + 1) & (TONES - 1);

                    t.x_freq = x_freq;
                    t.f_delt = f_delt;
                    t.ph_rot = ph_rot;

                    for ch in 0..self.nchannels {
                        t.amp[ch] = if amp[ch] < AMP_MAX { amp[ch] as u8 } else { 0 };
                        t.phs[ch] = 128u32
                            .wrapping_sub(phs[ch].wrapping_mul(32))
                            .wrapping_add(shift as u32) as u8;
                    }
                }

                freq += 1;
            };

            self.tonal_bounds[group][sf_idx][1] = self.ntones as u16;

            sf += if diff != 0 { 8 } else { 1 };
        }

        Ok(())
    }

    fn parse_tonal_chunk(&mut self, chunk: &Chunk<'_>) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);

        // Scale factors.
        if chunk.id == CHUNK_SCF || chunk.id == CHUNK_TONAL_SCF {
            if gb.left() < 36 {
                return Err(Invalid);
            }
            for sb in 0..6 {
                self.tonal_scf[sb] = gb.get(6) as u8;
            }
        }

        // Tonal groups.
        if chunk.id == CHUNK_TONAL || chunk.id == CHUNK_TONAL_SCF {
            for group in 0..5 {
                self.parse_tonal(&mut gb, group)?;
            }
        }

        Ok(())
    }

    fn parse_tonal_group(&mut self, chunk: &Chunk<'_>) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);
        self.parse_tonal(&mut gb, chunk.id)
    }

    /// Parse the scale factors of a grid 1 subband: the interpolation of 8 values from a
    /// handful of coded points.
    fn parse_scale_factors(&mut self, gb: &mut LeBits<'_>, ch: usize, g1_sb: usize) -> Res {
        let huff = Huffman::get();
        let mut scf = [0u8; 8];

        let res = Self::parse_scale_factors_into(huff, gb, &mut scf);
        self.grid_1_scf[ch][g1_sb] = scf;
        res
    }

    fn parse_scale_factors_into(huff: &Huffman, gb: &mut LeBits<'_>, scf: &mut [u8; 8]) -> Res {
        // Truncated scale factors remain zero.
        if gb.ensure(20) {
            return Ok(());
        }

        // Initial scale factor.
        let mut prev = huff.fst_rsd_amp.parse(gb);
        let mut next = 0;
        let mut sf = 0usize;

        while sf < 7 {
            scf[sf] = prev as u8;

            if gb.ensure(20) {
                return Ok(());
            }

            // Interpolation distance.
            let dist = huff.rsd_apprx.parse(gb) + 1;
            if dist > 7 - sf as i32 {
                return Err(Invalid);
            }
            let dist = dist as usize;

            if gb.ensure(20) {
                return Ok(());
            }

            // Final interpolation point.
            next = huff.rsd_amp.parse(gb);

            if next & 1 != 0 {
                next = prev + ((next + 1) >> 1);
            }
            else {
                next = prev - (next >> 1);
            }

            // Interpolate.
            match dist {
                2 => {
                    scf[sf + 1] = if next > prev {
                        prev + ((next - prev) >> 1)
                    }
                    else {
                        prev - ((prev - next) >> 1)
                    } as u8;
                }
                4 => {
                    if next > prev {
                        scf[sf + 1] = (prev + ((next - prev) >> 2)) as u8;
                        scf[sf + 2] = (prev + ((next - prev) >> 1)) as u8;
                        scf[sf + 3] = (prev + (((next - prev) * 3) >> 2)) as u8;
                    }
                    else {
                        scf[sf + 1] = (prev - ((prev - next) >> 2)) as u8;
                        scf[sf + 2] = (prev - ((prev - next) >> 1)) as u8;
                        scf[sf + 3] = (prev - (((prev - next) * 3) >> 2)) as u8;
                    }
                }
                _ => {
                    for i in 1..dist {
                        scf[sf + i] = (prev + (next - prev) * i as i32 / dist as i32) as u8;
                    }
                }
            }

            prev = next;
            sf += dist;
        }

        // Final value.
        scf[sf] = next as u8;

        Ok(())
    }

    fn parse_st_code(gb: &mut LeBits<'_>, min_v: u32) -> u8 {
        let mut v = (Huffman::get().st_grid.parse(gb) as u32).wrapping_add(min_v);

        if v & 1 != 0 {
            v = 16 + (v >> 1);
        }
        else {
            v = 16u32.wrapping_sub(v >> 1);
        }

        if v as usize >= ST_COEFF.len() {
            v = 16;
        }
        v as u8
    }

    fn parse_grid_1_chunk(&mut self, chunk: &Chunk<'_>, ch1: usize, ch2: usize) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let huff = Huffman::get();
        let mut gb = LeBits::new(chunk.data);
        let gb = &mut gb;

        // Scale factors.
        let nsubbands = usize::from(SCF_TO_GRID_1[self.nsubbands - 1]) + 1;
        for sb in 2..nsubbands {
            self.parse_scale_factors(gb, ch1, sb)?;
            if ch1 != ch2 && usize::from(GRID_1_TO_SCF[sb]) < self.min_mono_subband {
                self.parse_scale_factors(gb, ch2, sb)?;
            }
        }

        if gb.left() < 1 {
            // Should not happen, but a sample exists that proves otherwise.
            return Ok(());
        }

        // Average values for the third grid.
        for sb in 0..self.nsubbands - 4 {
            self.grid_3_avg[ch1][sb] = (huff.avg_g3.parse(gb) - 16) as i8;
            if ch1 != ch2 {
                if sb + 4 < self.min_mono_subband {
                    self.grid_3_avg[ch2][sb] = (huff.avg_g3.parse(gb) - 16) as i8;
                }
                else {
                    self.grid_3_avg[ch2][sb] = self.grid_3_avg[ch1][sb];
                }
            }
        }

        if gb.left() < 0 {
            return Err(Invalid);
        }

        // Stereo image for partial mono mode.
        if ch1 != ch2 {
            if gb.ensure(8) {
                return Ok(());
            }

            let min_v = [gb.get(4), gb.get(4)];

            let nsubbands = (self.nsubbands - self.min_mono_subband).div_ceil(4);
            for sb in 0..nsubbands {
                for ch in ch1..=ch2 {
                    for sf in 1..=4 {
                        self.part_stereo[ch][sb][sf] = Self::parse_st_code(gb, min_v[ch - ch1]);
                    }
                }
            }

            if gb.left() >= 0 {
                self.part_stereo_pres |= 1 << ch1;
            }
        }

        // Low resolution spatial information is not decoded.

        Ok(())
    }

    fn parse_grid_1_sec_ch(&mut self, gb: &mut LeBits<'_>, ch2: usize) -> Res {
        let huff = Huffman::get();

        // Scale factors.
        let nsubbands = usize::from(SCF_TO_GRID_1[self.nsubbands - 1]) + 1;
        for sb in 2..nsubbands {
            if usize::from(GRID_1_TO_SCF[sb]) >= self.min_mono_subband {
                self.parse_scale_factors(gb, ch2, sb)?;
            }
        }

        // Average values for the third grid.
        for sb in 0..self.nsubbands - 4 {
            if sb + 4 >= self.min_mono_subband {
                if gb.ensure(20) {
                    return Ok(());
                }
                self.grid_3_avg[ch2][sb] = (huff.avg_g3.parse(gb) - 16) as i8;
            }
        }

        Ok(())
    }

    fn parse_grid_3(&mut self, gb: &mut LeBits<'_>, ch1: usize, ch2: usize, sb: usize, flag: bool) {
        let huff = Huffman::get();

        for ch in ch1..=ch2 {
            if (ch != ch1 && sb + 4 >= self.min_mono_subband) != flag {
                continue;
            }

            if self.grid_3_pres[ch] & (1 << sb) != 0 {
                // Already parsed.
                continue;
            }

            for i in 0..8 {
                if gb.ensure(20) {
                    return;
                }
                self.grid_3_scf[ch][sb][i] = (huff.grid_3.parse(gb) - 16) as i8;
            }

            // Flag the scale factors of this subband parsed.
            self.grid_3_pres[ch] |= 1 << sb;
        }
    }

    /// Parse the time samples of one subband, filling truncated samples with randomness.
    fn parse_ch(&mut self, gb: &mut LeBits<'_>, ch: usize, sb: usize, quant_level: u8, flag: bool) {
        let huff = Huffman::get();
        let base = Self::ts_base(ch, sb);

        if gb.ensure(20) {
            // Too few bits left.
            return;
        }

        let coding_method = gb.get1();

        let mut i = 0usize;

        // The samples including the padding.
        let samples = &mut self.time_samples[base..base + TS_STRIDE - TIME_HISTORY];

        match quant_level {
            1 => {
                let nblocks = ((gb.left() / 8) as usize).min(TIME_SAMPLES / 8);
                for _ in 0..nblocks {
                    let code = gb.get(8);
                    for j in 0..8 {
                        samples[i + j] = RSD_LEVEL_2A[((code >> j) & 1) as usize];
                    }
                    i += 8;
                }
            }
            2 => {
                if coding_method {
                    while i < TIME_SAMPLES && gb.left() >= 2 {
                        samples[i] =
                            if gb.get1() { RSD_LEVEL_2B[usize::from(gb.get1())] } else { 0.0 };
                        i += 1;
                    }
                }
                else {
                    let nblocks = ((gb.left() / 8) as usize).min(TIME_SAMPLES.div_ceil(5));
                    for _ in 0..nblocks {
                        let code = RSD_PACK_5_IN_8[gb.get(8) as usize];
                        for j in 0..5 {
                            samples[i + j] = RSD_LEVEL_3[usize::from((code >> (j * 2)) & 3)];
                        }
                        i += 5;
                    }
                }
            }
            3 => {
                let nblocks = ((gb.left() / 7) as usize).min(TIME_SAMPLES.div_ceil(3));
                for _ in 0..nblocks {
                    let code = gb.get(7) as usize;
                    for j in 0..3 {
                        samples[i + j] = RSD_LEVEL_5[usize::from(RSD_PACK_3_IN_7[code][j])];
                    }
                    i += 3;
                }
            }
            4 => {
                while i < TIME_SAMPLES && gb.left() >= 6 {
                    samples[i] = RSD_LEVEL_8[huff.rsd.decode(gb).max(0) as usize];
                    i += 1;
                }
            }
            5 => {
                let nblocks = ((gb.left() / 4) as usize).min(TIME_SAMPLES);
                for _ in 0..nblocks {
                    samples[i] = RSD_LEVEL_16[gb.get(4) as usize];
                    i += 1;
                }
            }
            _ => (),
        }

        if flag && gb.left() < 20 {
            // Skip an incomplete mono subband.
            return;
        }

        while i < TIME_SAMPLES {
            let r = self.lbr_rand(sb);
            self.time_samples[base + i] = r;
            i += 1;
        }

        self.ch_pres[ch] |= 1 << sb;
    }

    fn parse_ts(
        &mut self,
        gb: &mut LeBits<'_>,
        ch1: usize,
        ch2: usize,
        start_sb: usize,
        end_sb: usize,
        flag: bool,
    ) -> Res {
        for sb in start_sb..end_sb {
            // Subband number before reordering.
            let sb_reorder = if sb < 6 {
                sb
            }
            else if flag && sb < self.max_mono_subband {
                usize::from(self.sb_indices[sb])
            }
            else {
                if gb.ensure(28) {
                    break;
                }
                let r = (gb.get(self.limited_range as u32 + 3) as usize).max(6);
                self.sb_indices[sb] = r as u8;
                r
            };

            if sb_reorder >= self.nsubbands {
                return Err(Invalid);
            }

            // Third grid scale factors.
            if sb == 12 {
                for sb_g3 in 0..self.g3_avg_only_start_sb.saturating_sub(4) {
                    self.parse_grid_3(gb, ch1, ch2, sb_g3, flag);
                }
            }
            else if sb < 12 && sb_reorder >= 4 {
                self.parse_grid_3(gb, ch1, ch2, sb_reorder - 4, flag);
            }

            // Secondary channel flags.
            if ch1 != ch2 {
                if gb.ensure(20) {
                    break;
                }
                if !flag || sb_reorder >= self.max_mono_subband {
                    self.sec_ch_sbms[ch1 / 2][sb_reorder] = gb.get(8) as u8;
                }
                if flag && sb_reorder >= self.min_mono_subband {
                    self.sec_ch_lrms[ch1 / 2][sb_reorder] = gb.get(8) as u8;
                }
            }

            let quant_level = self.quant_levels[ch1 / 2][sb];
            if quant_level == 0 {
                return Err(Invalid);
            }

            // Time samples for one or both channels.
            if sb < self.max_mono_subband && sb_reorder >= self.min_mono_subband {
                if !flag {
                    self.parse_ch(gb, ch1, sb_reorder, quant_level, false);
                }
                else if ch1 != ch2 {
                    self.parse_ch(gb, ch2, sb_reorder, quant_level, true);
                }
            }
            else {
                self.parse_ch(gb, ch1, sb_reorder, quant_level, false);
                if ch1 != ch2 {
                    self.parse_ch(gb, ch2, sb_reorder, quant_level, false);
                }
            }
        }

        Ok(())
    }

    /// Convert from reflection coefficients to direct form coefficients.
    fn convert_lpc(coeff: &mut [f32; 8], codes: &[usize]) {
        for i in 0..8 {
            let rc = LPC_TAB[codes[i]];
            for j in 0..i.div_ceil(2) {
                let tmp1 = coeff[j];
                let tmp2 = coeff[i - j - 1];
                coeff[j] = tmp1 + rc * tmp2;
                coeff[i - j - 1] = tmp2 + rc * tmp1;
            }
            coeff[i] = rc;
        }
    }

    fn parse_lpc(
        &mut self,
        gb: &mut LeBits<'_>,
        ch1: usize,
        ch2: usize,
        start_sb: usize,
        end_sb: usize,
    ) {
        let f = self.framenum & 1;
        let mut codes = [0usize; 16];

        // The first two subbands have two sets of coefficients, the third subband has one.
        for sb in start_sb..end_sb {
            let ncodes = 8 * (1 + usize::from(sb < 2));
            for ch in ch1..=ch2 {
                if gb.ensure(4 * ncodes as i64) {
                    return;
                }
                for code in &mut codes[..ncodes] {
                    *code = gb.get(4) as usize;
                }
                for i in 0..ncodes / 8 {
                    Self::convert_lpc(&mut self.lpc_coeff[f][ch][sb][i], &codes[i * 8..]);
                }
            }
        }
    }

    fn parse_high_res_grid(&mut self, chunk: &Chunk<'_>, ch1: usize, ch2: usize) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);
        let gb = &mut gb;

        // Quantizer profile.
        let profile = gb.get(8) as usize;
        // Overall level.
        let ol = ((profile >> 3) & 7) as i32;
        // Steepness.
        let st = (profile >> 6) as i32;
        // Max energy subband.
        let max_sb = profile & 7;

        // Calculate the quantization levels.
        let mut quant_levels = [0u8; SUBBANDS];
        for sb in 0..self.nsubbands {
            let f = (sb as u32 * self.limited_rate / self.nsubbands as u32) as i32;
            let a = 18000 / (12 * f / 1000 + 100 + 40 * st) + 20 * ol;
            quant_levels[sb] = match a {
                ..=95 => 1,
                96..=140 => 2,
                141..=180 => 3,
                181..=230 => 4,
                _ => 5,
            };
        }

        // Reorder the quantization levels of the lower subbands.
        for sb in 0..8 {
            self.quant_levels[ch1 / 2][sb] = quant_levels[usize::from(SB_REORDER[max_sb][sb])];
        }
        self.quant_levels[ch1 / 2][8..self.nsubbands]
            .copy_from_slice(&quant_levels[8..self.nsubbands]);

        // LPC for the first two subbands.
        self.parse_lpc(gb, ch1, ch2, 0, 2);

        // Time samples for the first two subbands of the main channel.
        self.parse_ts(gb, ch1, ch2, 0, 2, false)?;

        // First two bands of the first grid.
        for sb in 0..2 {
            for ch in ch1..=ch2 {
                self.parse_scale_factors(gb, ch, sb)?;
            }
        }

        Ok(())
    }

    fn parse_grid_2(
        &mut self,
        gb: &mut LeBits<'_>,
        ch1: usize,
        ch2: usize,
        start_sb: usize,
        end_sb: usize,
        flag: bool,
    ) {
        let huff = Huffman::get();

        let nsubbands = usize::from(SCF_TO_GRID_2[self.nsubbands - 1]) + 1;
        let end_sb = end_sb.min(nsubbands);

        for sb in start_sb..end_sb {
            for ch in ch1..=ch2 {
                if (ch != ch1 && usize::from(GRID_2_TO_SCF[sb]) >= self.min_mono_subband) != flag {
                    if !flag {
                        self.grid_2_scf[ch][sb] = self.grid_2_scf[ch1][sb];
                    }
                    continue;
                }

                // Scale factors in groups of 8.
                for i in 0..8 {
                    let g2_scf = &mut self.grid_2_scf[ch][sb][i * 8..i * 8 + 8];

                    if gb.left() < 1 {
                        self.grid_2_scf[ch][sb][i * 8..].fill(0);
                        break;
                    }

                    // Bit indicating if the whole group has zero values.
                    if gb.get1() {
                        for v in g2_scf.iter_mut() {
                            if gb.ensure(20) {
                                break;
                            }
                            *v = huff.grid_2.parse(gb) as u8;
                        }
                    }
                    else {
                        g2_scf.fill(0);
                    }
                }
            }
        }
    }

    fn parse_ts1_chunk(&mut self, chunk: &Chunk<'_>, ch1: usize, ch2: usize) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);
        let gb = &mut gb;

        self.parse_lpc(gb, ch1, ch2, 2, 3);
        self.parse_ts(gb, ch1, ch2, 2, 4, false)?;
        self.parse_grid_2(gb, ch1, ch2, 0, 1, false);
        self.parse_ts(gb, ch1, ch2, 4, 6, false)
    }

    fn parse_ts2_chunk(&mut self, chunk: &Chunk<'_>, ch1: usize, ch2: usize) -> Res {
        if chunk.data.is_empty() {
            return Ok(());
        }

        let mut gb = LeBits::new(chunk.data);
        let gb = &mut gb;

        self.parse_grid_2(gb, ch1, ch2, 1, 3, false);
        self.parse_ts(gb, ch1, ch2, 6, self.max_mono_subband, false)?;

        if ch1 != ch2 {
            self.parse_grid_1_sec_ch(gb, ch2)?;
            self.parse_grid_2(gb, ch1, ch2, 0, 3, true);
        }

        self.parse_ts(gb, ch1, ch2, self.min_mono_subband, self.nsubbands, true)
    }

    // ---------------------------------------------------------------------------------------
    // Reconstruction
    // ---------------------------------------------------------------------------------------

    /// Reconstruct the high-frequency resolution grid from the first and third grids.
    fn decode_grid(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in 0..self.nsubbands {
                let g1_sb = usize::from(SCF_TO_GRID_1[sb]);

                let a = self.grid_1_scf[ch][g1_sb];
                let b = self.grid_1_scf[ch][g1_sb + 1];

                let w1 = i32::from(GRID_1_WEIGHTS[g1_sb][sb]);
                let w2 = i32::from(GRID_1_WEIGHTS[g1_sb + 1][sb]);

                for i in 0..8 {
                    let scf = (w1 * i32::from(a[i]) + w2 * i32::from(b[i])) >> 7;
                    self.high_res_scf[ch][sb][i] = if sb < 4 {
                        scf as u8
                    }
                    else {
                        let g3_avg = i32::from(self.grid_3_avg[ch][sb - 4]);
                        let g3_scf = i32::from(self.grid_3_scf[ch][sb - 4][i]);
                        (scf - g3_avg - g3_scf) as u8
                    };
                }
            }
        }
    }

    /// Fill unallocated subbands with randomness.
    fn random_ts(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in 0..self.nsubbands {
                if self.ch_pres[ch] & (1 << sb) != 0 {
                    // Skip allocated subbands.
                    continue;
                }

                let base = Self::ts_base(ch, sb);

                if sb < 2 {
                    // The first two subbands are always zero.
                    self.time_samples[base..base + TIME_SAMPLES].fill(0.0);
                }
                else if sb < 10 {
                    for i in 0..TIME_SAMPLES {
                        let r = self.lbr_rand(sb);
                        self.time_samples[base + i] = r;
                    }
                }
                else {
                    for blk in 0..TIME_SAMPLES / 8 {
                        let mut accum = [0f32; 8];

                        // Modulate by subbands 2-5 in blocks of 8.
                        for k in 2..6 {
                            let other = Self::ts_base(ch, k) + blk * 8;
                            for j in 0..8 {
                                accum[j] += self.time_samples[other + j].abs();
                            }
                        }

                        for j in 0..8 {
                            let r = self.lbr_rand(sb);
                            self.time_samples[base + blk * 8 + j] = (accum[j] * 0.25 + 0.5) * r;
                        }
                    }
                }
            }
        }
    }

    /// Inverse prediction in place.
    fn predict(ts: &mut [f32], base: usize, coeff: &[f32; 8], nsamples: usize) {
        for i in 0..nsamples {
            let mut res = 0.0f32;
            for j in 0..8 {
                res += coeff[j] * ts[base + i - j - 1];
            }
            ts[base + i] -= res;
        }
    }

    fn synth_lpc(&mut self, ch1: usize, ch2: usize, sb: usize) {
        let f = self.framenum & 1;

        for ch in ch1..=ch2 {
            if self.ch_pres[ch] & (1 << sb) == 0 {
                continue;
            }

            let base = Self::ts_base(ch, sb);
            let ts = &mut self.time_samples;
            let lpc = &self.lpc_coeff;

            if sb < 2 {
                Self::predict(ts, base, &lpc[f ^ 1][ch][sb][1], 16);
                Self::predict(ts, base + 16, &lpc[f][ch][sb][0], 64);
                Self::predict(ts, base + 80, &lpc[f][ch][sb][1], 48);
            }
            else {
                Self::predict(ts, base, &lpc[f ^ 1][ch][sb][0], 16);
                Self::predict(ts, base + 16, &lpc[f][ch][sb][0], 112);
            }
        }
    }

    fn filter_ts(&mut self, ch1: usize, ch2: usize) {
        for sb in 0..self.nsubbands {
            // Scale factors.
            for ch in ch1..=ch2 {
                let base = Self::ts_base(ch, sb);
                let samples = &mut self.time_samples[base..base + TIME_SAMPLES];
                let hr_scf = &self.high_res_scf[ch][sb];

                if sb < 4 {
                    for (i, blk) in samples.chunks_exact_mut(16).enumerate() {
                        let scf = u32::from(hr_scf[i]).min(AMP_MAX);
                        for s in blk {
                            *s *= QUANT_AMP[scf as usize];
                        }
                    }
                }
                else {
                    let g2_scf = &self.grid_2_scf[ch][usize::from(SCF_TO_GRID_2[sb])];
                    for (i, pair) in samples.chunks_exact_mut(2).enumerate() {
                        let scf = (i32::from(hr_scf[i / 8]) - i32::from(g2_scf[i])) as u32;
                        let amp = QUANT_AMP[scf.min(AMP_MAX) as usize];
                        pair[0] *= amp;
                        pair[1] *= amp;
                    }
                }
            }

            // Mid-side stereo.
            if ch1 != ch2 {
                let base_l = Self::ts_base(ch1, sb);
                let base_r = Self::ts_base(ch2, sb);
                let ch2_pres = self.ch_pres[ch2] & (1 << sb) != 0;

                for i in 0..TIME_SAMPLES / 16 {
                    let sbms = (self.sec_ch_sbms[ch1 / 2][sb] >> i) & 1 != 0;
                    let lrms = (self.sec_ch_lrms[ch1 / 2][sb] >> i) & 1 != 0;

                    for j in i * 16..i * 16 + 16 {
                        let l = self.time_samples[base_l + j];
                        let r = self.time_samples[base_r + j];

                        if sb >= self.min_mono_subband {
                            if lrms && ch2_pres {
                                if sbms {
                                    self.time_samples[base_l + j] = r;
                                    self.time_samples[base_r + j] = -l;
                                }
                                else {
                                    self.time_samples[base_l + j] = r;
                                    self.time_samples[base_r + j] = l;
                                }
                            }
                            else if !ch2_pres {
                                if sbms && self.part_stereo_pres & (1 << ch1) != 0 {
                                    self.time_samples[base_r + j] = -l;
                                }
                                else {
                                    self.time_samples[base_r + j] = l;
                                }
                            }
                        }
                        else if sbms && ch2_pres {
                            self.time_samples[base_l + j] = (l + r) * 0.5;
                            self.time_samples[base_r + j] = (l - r) * 0.5;
                        }
                    }
                }
            }

            // Inverse prediction.
            if sb < 3 {
                self.synth_lpc(ch1, ch2, sb);
            }
        }
    }

    /// Modulate by the interpolated partial stereo coefficients.
    fn decode_part_stereo(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in self.min_mono_subband..self.nsubbands {
                let pt_st = self.part_stereo[ch][(sb - self.min_mono_subband) / 4];
                let base = Self::ts_base(ch, sb);

                if self.ch_pres[ch2] & (1 << sb) != 0 {
                    continue;
                }

                for sf in 1..=4 {
                    let prev = ST_COEFF[usize::from(pt_st[sf - 1])];
                    let next = ST_COEFF[usize::from(pt_st[sf])];

                    let samples = &mut self.time_samples[base + (sf - 1) * 32..][..32];
                    for (i, s) in samples.iter_mut().enumerate() {
                        *s *= (32 - i) as f32 * prev + i as f32 * next;
                    }
                }
            }
        }
    }

    /// Synthesise the tones in the given group for the given tonal subframe.
    fn synth_tones(
        &mut self,
        ch: usize,
        values: &mut [f32],
        group: usize,
        group_sf: i32,
        synth_idx: i32,
    ) {
        if synth_idx < 0 {
            return;
        }

        let bounds = self.tonal_bounds[group][group_sf as usize];
        let start = usize::from(bounds[0]);
        let count = (usize::from(bounds[1]).wrapping_sub(start)) & (TONES - 1);

        for i in 0..count {
            let t = &mut self.tones[(start + i) & (TONES - 1)];

            if t.amp[ch] != 0 {
                let amp = SYNTH_ENV[synth_idx as usize] * QUANT_AMP[usize::from(t.amp[ch])];
                let c = amp * COS_TAB[usize::from(t.phs[ch])];
                let s = amp * COS_TAB[usize::from(t.phs[ch].wrapping_add(64))];
                let cf = &CORR_CF[usize::from(t.f_delt)];
                let x_freq = isize::from(t.x_freq);

                // The eleven taps around the spectral line: the taps below the first line are
                // reflected, except for line 0 where they are dropped.
                let taps = [-s, c, s, -c, -s, c, s, -c, -s, c, s];
                for (k, (&tap, &coeff)) in taps.iter().zip(cf.iter()).enumerate() {
                    let idx = x_freq + k as isize - 5;
                    let idx = if idx >= 0 {
                        idx as usize
                    }
                    else if x_freq == 0 {
                        continue;
                    }
                    else {
                        (-1 - idx) as usize
                    };
                    values[idx] += coeff * tap;
                }
            }

            t.phs[ch] = t.phs[ch].wrapping_add(t.ph_rot);
        }
    }

    /// Synthesise all tones in all groups for the given residual subframe.
    fn base_func_synth(&mut self, ch: usize, values: &mut [f32], sf: usize) {
        // The tonal versus residual shift is 22 subframes.
        for group in 0..5 {
            let sf22 = sf as i32 - 22;
            let group_sf = ((self.framenum << group) as i32) + (sf22 >> (5 - group));
            let synth_idx = ((((sf22 & 31) << group) & 31) + (1 << group)) - 1;

            self.synth_tones(ch, values, group, (group_sf - 1) & 31, 30 - synth_idx);
            self.synth_tones(ch, values, group, group_sf & 31, synth_idx);
        }
    }

    /// Short window and 8 point forward MDCT of 4 samples per subband (the hybrid filter bank)
    /// followed by the aliasing cancellation for high frequencies.
    fn lbr_bank(&self, values: &mut [f32], ch: usize, ofs: usize) {
        let [sw0, sw1, sw2, sw3, c1, c2, c3, c4, al1, al2] = BANK_COEFF;
        let len = self.nsubbands;

        for i in 0..len {
            let src = &self.time_samples[Self::ts_base(ch, i) + ofs - 4..][..8];

            let a = src[0] * sw0 - src[3] * sw3;
            let b = src[1] * sw1 - src[2] * sw2;
            let c = src[6] * sw1 + src[5] * sw2;
            let d = src[7] * sw0 + src[4] * sw3;

            values[i * 4] = c1 * b - c2 * c + c4 * a - c3 * d;
            values[i * 4 + 1] = c1 * d - c2 * a - c4 * b - c3 * c;
            values[i * 4 + 2] = c3 * b + c2 * d - c4 * c + c1 * a;
            values[i * 4 + 3] = c3 * a - c2 * b + c4 * d - c1 * c;
        }

        // Aliasing cancellation for high frequencies.
        for i in 12..len.saturating_sub(1) {
            let a = values[i * 4 + 3] * al1;
            let b = values[(i + 1) * 4] * al1;
            values[i * 4 + 3] += b - a;
            values[(i + 1) * 4] -= b + a;
            let a = values[i * 4 + 2] * al2;
            let b = values[(i + 1) * 4 + 1] * al2;
            values[i * 4 + 2] += b - a;
            values[(i + 1) * 4 + 1] -= b + a;
        }
    }

    fn transform_channel(&mut self, ch: usize, output: &mut [f32]) {
        let nsubbands = self.nsubbands;
        let n = 32usize << self.freq_range;

        let mut values = [0f32; SUBBANDS * 4];
        let mut dct = vec![0f32; n];

        for sf in 0..TIME_SAMPLES / 4 {
            // Hybrid filter bank. Inactive subbands stay zero.
            self.lbr_bank(&mut values, ch, sf * 4);

            self.base_func_synth(ch, &mut values, sf);

            // Inverse MDCT of the N coefficients into 2N samples, from the DCT-IV.
            for (d, row) in dct.iter_mut().zip(self.dct.chunks_exact(n)) {
                let mut acc = [0f32; 8];
                for (r, x) in row.chunks_exact(8).zip(values[..n].chunks_exact(8)) {
                    for k in 0..8 {
                        acc[k] += r[k] * x[k];
                    }
                }
                *d = ((acc[0] + acc[4]) + (acc[1] + acc[5]))
                    + ((acc[2] + acc[6]) + (acc[3] + acc[7]));
            }

            // The first half of the 2N output samples and, reversed, the second half. With
            // m = i + N / 2: y[i] = C[m] for m < N, and y[N + i] = -C[N - 1 - i + N / 2 ...]. See
            // below for the exact unfolding.
            let out = &mut output[sf * n..(sf + 1) * n];
            let hist = &mut self.history[ch];
            for i in 0..n {
                // y[i]: i < N / 2: C[i + N / 2]; otherwise -C[3N / 2 - 1 - i].
                let y0 = if i < n / 2 { dct[i + n / 2] } else { -dct[3 * n / 2 - 1 - i] };
                out[i] = y0 * self.window[i] + hist[i];
            }
            for i in 0..n {
                // y[N + i]: i < N / 2: -C[N / 2 - 1 - i]; otherwise -C[i - N / 2].
                let y1 = if i < n / 2 { -dct[n / 2 - 1 - i] } else { -dct[i - n / 2] };
                hist[i] = y1 * self.window[n - 1 - i];
            }
        }

        // Update the history for the LPC and the forward MDCT.
        for sb in 0..nsubbands {
            let base = Self::ts_base(ch, sb);
            self.time_samples.copy_within(
                base + TIME_SAMPLES - TIME_HISTORY..base + TIME_SAMPLES,
                base - TIME_HISTORY,
            );
        }
    }

    /// Reconstruct the frame parsed last into [`LbrDecoder::out`].
    pub fn filter_frame(&mut self) -> LbrFrame {
        // The channels of the LBR bitstream are ordered: L, R, Ls, Rs, C.
        let mut speakers = Vec::with_capacity(CHANNELS);
        if self.ch_mask & 0x2 != 0 {
            speakers.extend([spkr::L, spkr::R]);
        }
        if self.ch_mask & 0x4 != 0 {
            speakers.extend([spkr::LS, spkr::RS]);
        }
        if self.ch_mask & 0x1 != 0 {
            speakers.push(spkr::C);
        }

        let mut ch_mask = speakers.iter().fold(0u32, |m, &s| m | 1 << s);

        let nsamples = 1024usize << self.freq_range;

        for ch in 0..self.nchannels.div_ceil(2) * 2 {
            let ch1 = ch & !1;
            if ch & 1 != 0 {
                continue;
            }

            let ch2 = (ch1 + 1).min(self.nchannels - 1);

            self.decode_grid(ch1, ch2);
            self.random_ts(ch1, ch2);
            self.filter_ts(ch1, ch2);

            if ch1 != ch2 && self.part_stereo_pres & (1 << ch1) != 0 {
                self.decode_part_stereo(ch1, ch2);
            }

            for c in [ch1, ch2] {
                let Some(&spk) = speakers.get(c)
                else {
                    continue;
                };
                if c == ch2 && ch1 == ch2 {
                    continue;
                }
                let mut out = std::mem::take(&mut self.out[spk]);
                out.resize(nsamples, 0.0);
                self.transform_channel(c, &mut out);
                self.out[spk] = out;
            }
        }

        // Interpolate the LFE channel.
        if self.flags & FLAG_LFE_PRESENT != 0 {
            let mut out = std::mem::take(&mut self.out[spkr::LFE1]);
            out.resize(nsamples, 0.0);
            self.lfe_iir(&mut out);
            self.out[spkr::LFE1] = out;
            ch_mask |= 1 << spkr::LFE1;
        }

        LbrFrame { nsamples, sample_rate: self.sample_rate, ch_mask }
    }

    fn lfe_iir(&mut self, output: &mut [f32]) {
        let factor = 16usize << self.freq_range;
        let hist = &mut self.lfe_history;
        let mut pos = 0;

        for &input in &self.lfe_data {
            let mut res = input;

            for _ in 0..factor {
                for k in 0..5 {
                    let iir = &LFE_IIR[k];
                    let tmp = hist[k][0] * iir[0] + hist[k][1] * iir[1] + res;
                    res = hist[k][0] * iir[2] + hist[k][1] * iir[3] + tmp;

                    hist[k][0] = hist[k][1];
                    hist[k][1] = tmp;
                }

                output[pos] = res;
                pos += 1;
                res = 0.0;
            }
        }
    }
}

/// `cos(pi * i / 128)`.
static COS_TAB: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut t = [0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        *v = (std::f64::consts::PI * i as f64 / 128.0).cos() as f32;
    }
    t
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_tables_build() {
        // Every code book must fit in the code space.
        let _ = Huffman::get();
    }

    #[test]
    fn verify_le_reader() {
        let mut gb = LeBits::new(&[0b1010_0101, 0xff]);
        assert_eq!(gb.get(4), 0b0101);
        assert_eq!(gb.get(4), 0b1010);
        assert_eq!(gb.get(8), 0xff);
        assert_eq!(gb.left(), 0);
        assert_eq!(gb.get(3), 0);
        assert_eq!(gb.left(), -3);
    }

    #[test]
    fn verify_vlc_bit_order() {
        // Codes are assigned sequentially: symbol 1 -> `0`, symbol 0 -> `10`, symbol 2 -> `11`,
        // and the first bit of a code is the first bit read from the stream.
        let vlc = LbrVlc::new(&[(1, 1), (0, 2), (2, 2)], 0);
        let mut gb = LeBits::new(&[0b0001_1010]);
        assert_eq!(vlc.decode(&mut gb), 1); // 0
        assert_eq!(vlc.decode(&mut gb), 0); // 10
        assert_eq!(vlc.decode(&mut gb), 2); // 11
        assert_eq!(vlc.decode(&mut gb), 1); // 0
    }

    #[test]
    fn verify_ceil_log2() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(6), 3);
        assert_eq!(ceil_log2(32), 5);
    }
}
