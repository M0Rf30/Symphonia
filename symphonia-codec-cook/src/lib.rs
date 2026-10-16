// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio Cook (Cooker, Gecko, RealAudio G2) decoder.
//!
//! Cook is a proprietary transform codec (an adaptation of ITU-T G.722.1) used by RealMedia. It
//! has no public specification; this decoder follows the structure of the FFmpeg `cook` decoder
//! (see `NOTICE`).
//!
//! # Stream parameters
//!
//! The decoder expects:
//!
//! * `extra_data`: the codec-specific data of the RealAudio header (the bytes that follow the
//!   `codecdata_length` field of the `.ra4`/`.ra5` header, 8 or more bytes). This is what FFmpeg's
//!   RealMedia demuxer passes to its decoder as `extradata`.
//! * `sample_rate` and `channels`.
//! * Packets that each hold exactly one already-deinterleaved frame of `sub_packet_size` bytes
//!   (FFmpeg's `block_align`). The length of the packet is used as the frame size.
//!
//! Each frame decodes to 256, 512 or 1024 samples per channel. As in FFmpeg, the first two frames
//! decoded after creation or [`AudioDecoder::reset`] only prime the overlap-add state and produce
//! no output.

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

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    Position, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_COOK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    CCPL_HUFFCOUNTS, CCPL_HUFFSYMS, CPLBAND, CPLSCALES, CVH_HUFFCOUNTS, CVH_HUFFSYMS, DITHER,
    ENVELOPE_QUANT_INDEX_HUFFCOUNTS, ENVELOPE_QUANT_INDEX_HUFFSYMS, EXPBITS, INVRADIX, KMAX,
    QUANT_CENTROID, VD, VPR,
};

/// Cook versions (the first word of the extra data).
const MONO: u32 = 0x100_0001;
const STEREO: u32 = 0x100_0002;
const JOINT_STEREO: u32 = 0x100_0003;
const MC_COOK: u32 = 0x200_0000;

/// Number of coefficients in a subband.
const SUBBAND_SIZE: usize = 20;
/// Maximum number of subpackets in a frame.
const MAX_SUBPACKETS: usize = 5;
/// Size of the largest decode buffer: (50 subbands + 3 joint-stereo start subbands) * 20.
const DECODE_BUFFER_LEN: usize = 1060;
/// The first two frames only prime the overlap-add state.
const DISCARDED_FRAMES: u32 = 2;

/// Every 32 bits of the coded data are XORed with this key (applied byte-wise, big-endian).
const DECODE_KEY: [u8; 4] = [0x37, 0xc5, 0x11, 0xf2];

/// A reader for the XOR-decoded, big-endian (most-significant bit first) subpacket data. Reads
/// beyond the loaded data return zero bits.
struct BitReader {
    data: Vec<u8>,
    /// Current position in bits.
    pos: usize,
    /// Number of valid bits.
    size: usize,
}

impl BitReader {
    fn new() -> Self {
        BitReader { data: Vec::new(), pos: 0, size: 0 }
    }

    /// Loads `src` (the first `ceil(size_bits / 8)` bytes of it), undoing the XOR scrambling.
    fn load(&mut self, src: &[u8], size_bits: usize) {
        let len = size_bits.div_ceil(8).min(src.len());
        self.data.clear();
        self.data.extend(src[..len].iter().enumerate().map(|(i, &b)| b ^ DECODE_KEY[i & 3]));
        self.pos = 0;
        self.size = size_bits;
    }

    /// Peeks the next `n` (<= 32) bits.
    fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let skip = (self.pos & 7) as u32;
        let mut window = 0u64;
        for i in 0..5 {
            window = (window << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        ((window >> (40 - skip - n)) & ((1u64 << n) - 1)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    fn read_bit(&mut self) -> bool {
        self.read(1) != 0
    }

    fn bits_left(&self) -> usize {
        self.size.saturating_sub(self.pos)
    }

    /// Counts consecutive one bits terminated by a zero bit (which is consumed), up to `limit`.
    fn read_unary(&mut self, limit: usize) -> usize {
        let mut n = 0;
        while n < limit && self.read_bit() {
            n += 1;
        }
        n
    }
}

/// A canonical Huffman code, as constructed by FFmpeg's `ff_vlc_init_from_lengths`: codes are
/// assigned in increasing order, shortest codes first, in the order of the symbol table.
struct Vlc {
    /// Exclusive upper bound of the 16-bit left-aligned codes of length <= `l`.
    limit: [u32; 17],
    /// First 16-bit left-aligned code of length `l`.
    base: [u32; 17],
    /// Index in `symbols` of the first code of length `l`.
    offset: [u32; 17],
    symbols: Box<[u16]>,
}

impl Vlc {
    /// `counts[i]` is the number of codes of length `i + 1`.
    fn new(counts: &[u8; 16], symbols: impl Iterator<Item = u16>) -> Result<Self> {
        let mut limit = [0u32; 17];
        let mut base = [0u32; 17];
        let mut offset = [0u32; 17];

        let mut code = 0u32;
        let mut index = 0u32;

        for len in 1..=16usize {
            let count = u32::from(counts[len - 1]);
            base[len] = code;
            offset[len] = index;
            code += count << (16 - len);
            limit[len] = code;
            index += count;
        }

        if code > 1 << 16 {
            return decode_error("cook: overdetermined vlc");
        }

        let symbols: Box<[u16]> = symbols.take(index as usize).collect();

        if symbols.len() != index as usize {
            return decode_error("cook: too few vlc symbols");
        }

        Ok(Vlc { limit, base, offset, symbols })
    }

    fn read(&self, br: &mut BitReader) -> Result<u16> {
        let v = br.peek(16);

        for len in 1..=16usize {
            if v < self.limit[len] {
                let index = self.offset[len] + ((v - self.base[len]) >> (16 - len));
                br.skip(len as u32);
                return match self.symbols.get(index as usize) {
                    Some(&s) => Ok(s),
                    None => decode_error("cook: invalid vlc"),
                };
            }
        }

        decode_error("cook: invalid vlc code")
    }
}

/// FFmpeg's additive lagged-Fibonacci generator (`av_lfg`), seeded with 0.
struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    fn new() -> Self {
        Lfg { state: tables::LFG_INITIAL_STATE, index: 0 }
    }

    fn next(&mut self) -> u32 {
        let a = self.state[(self.index.wrapping_sub(24) & 63) as usize]
            .wrapping_add(self.state[(self.index.wrapping_sub(55) & 63) as usize]);
        self.state[(self.index & 63) as usize] = a;
        self.index = self.index.wrapping_add(1);
        a
    }
}

/// Gain control points of the current and previous frame.
#[derive(Clone, Copy, Default)]
struct Gains {
    now: [i32; 9],
    previous: [i32; 9],
}

/// A subpacket: one mono channel, a pair of stereo channels or a pair of joint-stereo channels.
struct Subpacket {
    ch_idx: usize,
    num_channels: usize,
    subbands: usize,
    js_subband_start: usize,
    js_vlc_bits: u32,
    log2_numvector_size: u32,
    joint_stereo: bool,
    bits_per_subpdiv: u32,
    total_subbands: usize,
    numvector_size: usize,
    channel_coupling: Option<Vlc>,
    /// Size in bytes of this subpacket in the current frame.
    size: usize,
    bits_per_subpacket: usize,
    previous1: Box<[f32]>,
    previous2: Box<[f32]>,
    gains1: Gains,
    gains2: Gains,
}

/// Big-endian reader over the extra data that yields zero for any read that runs past the end
/// (and consumes the remainder), like FFmpeg's `bytestream2`.
struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl ByteReader<'_> {
    fn left(&self) -> usize {
        self.data.len() - self.pos
    }

    fn get<const N: usize>(&mut self) -> u32 {
        if self.left() < N {
            self.pos = self.data.len();
            return 0;
        }
        let v =
            self.data[self.pos..self.pos + N].iter().fold(0u32, |a, &b| (a << 8) | u32::from(b));
        self.pos += N;
        v
    }
}

/// Stream-wide decoder state that is shared between subpackets.
struct Dsp {
    /// Samples per channel per frame.
    n: usize,
    imdct: Imdct,
    /// Sine window scaled by `sqrt(2 / n)`.
    window: Box<[f32]>,
    /// Output of the IMDCT (2 * n samples).
    mdct_output: Box<[f32]>,
    gain_size_factor: usize,
    gain_table: [f32; 31],
    pow2tab: [f32; 127],
    rootpow2tab: [f32; 127],
    lfg: Lfg,
    envelope_vlc: Vec<Vlc>,
    sqvh_vlc: Vec<Vlc>,
    br: BitReader,
}

/// Decode buffers of the MLT coefficients.
struct Buffers {
    /// Joint-stereo coefficients (left and right interleaved by subband).
    b0: Box<[f32]>,
    /// First channel.
    b1: Box<[f32]>,
    /// Second channel.
    b2: Box<[f32]>,
}

/// The largest magnitude of a quantizer index.
const MAX_QUANT: i32 = 63;

/// Time-domain gain interpolation of one eighth of a frame.
fn interpolate(
    pow2tab: &[f32; 127],
    gain_table: &[f32; 31],
    buffer: &mut [f32],
    gain_index: i32,
    gain_index_next: i32,
) {
    let mut fc1 = pow2tab[(gain_index + 63) as usize];

    if gain_index == gain_index_next {
        // Static gain.
        for b in buffer.iter_mut() {
            *b *= fc1;
        }
    }
    else {
        // Smooth gain.
        let fc2 = gain_table[(15 + (gain_index_next - gain_index)) as usize];
        for b in buffer.iter_mut() {
            *b *= fc1;
            fc1 *= fc2;
        }
    }
}

/// Clip to `[0, 7]` (FFmpeg `av_clip_uintp2(a, 3)`).
fn clip3(a: i32) -> usize {
    a.clamp(0, 7) as usize
}

impl Dsp {
    fn new(n: usize) -> Result<Self> {
        // Fast way of computing 2^i and 2^(0.5*i) for -63 <= i < 64. (FFmpeg `init_pow2table`.)
        let mut pow2tab = [0f32; 127];
        let mut rootpow2tab = [0f32; 127];
        let exp2_tab = [1.0f32, std::f32::consts::SQRT_2];
        let mut exp2_val = 2f32.powi(-63);
        let mut root_val = 2f32.powi(-32);
        for i in -63i32..64 {
            if i & 1 == 0 {
                root_val *= 2.0;
            }
            pow2tab[(63 + i) as usize] = exp2_val;
            rootpow2tab[(63 + i) as usize] = root_val * exp2_tab[(i & 1) as usize];
            exp2_val *= 2.0;
        }

        let gain_size_factor = n / 8;
        let mut gain_table = [0f32; 31];
        for (i, g) in gain_table.iter_mut().enumerate() {
            *g = f64::from(pow2tab[i + 48]).powf(1.0 / gain_size_factor as f64) as f32;
        }

        // Simple sine window (FFmpeg `ff_sine_window_init`), scaled for the MLT.
        let scale = (2.0 / n as f64).sqrt();
        let window: Box<[f32]> = (0..n)
            .map(|i| {
                let w = ((i as f64 + 0.5) / (2.0 * n as f64) * std::f64::consts::PI).sin() as f32;
                (f64::from(w) * scale) as f32
            })
            .collect();

        let mut envelope_vlc = Vec::with_capacity(13);
        for i in 0..13 {
            envelope_vlc.push(Vlc::new(
                &ENVELOPE_QUANT_INDEX_HUFFCOUNTS[i],
                ENVELOPE_QUANT_INDEX_HUFFSYMS[i].iter().map(|&s| u16::from(s)),
            )?);
        }

        let mut sqvh_vlc = Vec::with_capacity(7);
        for i in 0..7 {
            sqvh_vlc.push(Vlc::new(&CVH_HUFFCOUNTS[i], CVH_HUFFSYMS[i].iter().copied())?);
        }

        Ok(Dsp {
            n,
            imdct: Imdct::new_scaled(n, 1.0 / 32768.0),
            window,
            mdct_output: vec![0.0; 2 * n].into_boxed_slice(),
            gain_size_factor,
            gain_table,
            pow2tab,
            rootpow2tab,
            lfg: Lfg::new(),
            envelope_vlc,
            sqvh_vlc,
            br: BitReader::new(),
        })
    }

    /// Fills the gain array for the time-domain quantization.
    fn decode_gain_info(&mut self, gains: &mut [i32; 9]) {
        // The amount of elements * 2 to update.
        let mut n = self.br.read_unary(self.br.bits_left());

        let mut i = 0;
        while n > 0 {
            n -= 1;
            let index = self.br.read(3) as usize;
            let gain = if self.br.read_bit() { self.br.read(4) as i32 - 7 } else { -1 };

            while i <= index {
                gains[i] = gain;
                i += 1;
            }
        }
        while i <= 8 {
            gains[i] = 0;
            i += 1;
        }
    }

    /// Loads the (XOR-scrambled) subpacket data and reads the gain info.
    fn decode_bytes_and_gain(&mut self, p: &Subpacket, data: &[u8], gains: &mut Gains) {
        self.br.load(data, p.bits_per_subpacket);
        self.decode_gain_info(&mut gains.now);

        // Swap current and previous gains.
        std::mem::swap(&mut gains.now, &mut gains.previous);
    }

    /// Creates the quantizer index table needed for the envelope.
    fn decode_envelope(&mut self, p: &Subpacket, quant_index_table: &mut [i32; 102]) -> Result<()> {
        quant_index_table[0] = self.br.read(6) as i32 - 6;

        for i in 1..p.total_subbands {
            let mut vlc_index = i;
            if i >= p.js_subband_start * 2 {
                vlc_index -= p.js_subband_start;
            }
            else {
                vlc_index /= 2;
                if vlc_index < 1 {
                    vlc_index = 1;
                }
            }
            // The VLC tables > 13 are identical to No. 13.
            if vlc_index > 13 {
                vlc_index = 13;
            }

            // The envelope symbols are biased by 12.
            let j = i32::from(self.envelope_vlc[vlc_index - 1].read(&mut self.br)?) - 12;
            quant_index_table[i] = quant_index_table[i - 1] + j;

            if quant_index_table[i] > MAX_QUANT || quant_index_table[i] < -MAX_QUANT {
                return decode_error("cook: quantizer out of range");
            }
        }

        Ok(())
    }

    /// Calculates the category and category index vectors.
    fn categorize(
        &self,
        p: &Subpacket,
        quant_index_table: &[i32; 102],
        category: &mut [i32; 128],
        category_index: &mut [i32; 128],
    ) {
        let spc = self.n as i32;

        let mut exp_index1 = [0i32; 102];
        let mut exp_index2 = [0i32; 102];

        let mut tmp_categorize_array = [0i32; 128 * 2];
        let mut tmp_categorize_array1_idx = p.numvector_size;
        let mut tmp_categorize_array2_idx = p.numvector_size;

        let mut bits_left = p.bits_per_subpacket as i32 - self.br.pos as i32;

        if bits_left > spc {
            bits_left = spc + ((bits_left - spc) * 5) / 8;
        }

        let mut bias = -32;

        // Estimate bias.
        let mut i = 32;
        while i > 0 {
            let mut num_bits = 0;
            for &quant_index in quant_index_table.iter().take(p.total_subbands) {
                let exp_idx = clip3((i - quant_index + bias) / 2);
                num_bits += EXPBITS[exp_idx];
            }
            if num_bits >= bits_left - 32 {
                bias += i;
            }
            i /= 2;
        }

        // Calculate total number of bits.
        let mut num_bits = 0;
        for i in 0..p.total_subbands {
            let exp_idx = clip3((bias - quant_index_table[i]) / 2);
            num_bits += EXPBITS[exp_idx];
            exp_index1[i] = exp_idx as i32;
            exp_index2[i] = exp_idx as i32;
        }
        let mut tmpbias1 = num_bits;
        let mut tmpbias2 = num_bits;

        for _ in 1..p.numvector_size {
            if tmpbias1 + tmpbias2 > 2 * bits_left {
                // --->
                let mut max = -999999;
                let mut index = None;
                for i in 0..p.total_subbands {
                    if exp_index1[i] < 7 {
                        let v = (-2 * exp_index1[i]) - quant_index_table[i] + bias;
                        if v >= max {
                            max = v;
                            index = Some(i);
                        }
                    }
                }
                let Some(index) = index
                else {
                    break;
                };
                tmp_categorize_array[tmp_categorize_array1_idx] = index as i32;
                tmp_categorize_array1_idx += 1;
                let e = exp_index1[index] as usize;
                tmpbias1 -= EXPBITS[e] - EXPBITS[e + 1];
                exp_index1[index] += 1;
            }
            else {
                // <---
                let mut min = 999999;
                let mut index = None;
                for i in 0..p.total_subbands {
                    if exp_index2[i] > 0 {
                        let v = (-2 * exp_index2[i]) - quant_index_table[i] + bias;
                        if v < min {
                            min = v;
                            index = Some(i);
                        }
                    }
                }
                let Some(index) = index
                else {
                    break;
                };
                tmp_categorize_array2_idx -= 1;
                tmp_categorize_array[tmp_categorize_array2_idx] = index as i32;
                let e = exp_index2[index] as usize;
                tmpbias2 -= EXPBITS[e] - EXPBITS[e - 1];
                exp_index2[index] -= 1;
            }
        }

        category[..p.total_subbands].copy_from_slice(&exp_index2[..p.total_subbands]);

        for ci in category_index.iter_mut().take(p.numvector_size - 1) {
            *ci = tmp_categorize_array[tmp_categorize_array2_idx];
            tmp_categorize_array2_idx += 1;
        }
    }

    /// Unpacks the coefficient index and sign vectors of a subband. Returns true if the data ran
    /// out.
    fn unpack_sqvh(
        &mut self,
        p: &Subpacket,
        category: usize,
        coef_index: &mut [i32; SUBBAND_SIZE],
        coef_sign: &mut [bool; SUBBAND_SIZE],
    ) -> Result<bool> {
        let vd = VD[category] as usize;
        let mut result = false;

        for i in 0..VPR[category] as usize {
            let mut vlc = i32::from(self.sqvh_vlc[category].read(&mut self.br)?);

            if p.bits_per_subpacket < self.br.pos {
                vlc = 0;
                result = true;
            }

            for j in (0..vd).rev() {
                let tmp = (vlc * INVRADIX[category]) / 0x100000;
                coef_index[vd * i + j] = vlc - tmp * (KMAX[category] + 1);
                vlc = tmp;
            }

            for j in 0..vd {
                if coef_index[i * vd + j] != 0 {
                    if self.br.pos < p.bits_per_subpacket {
                        coef_sign[i * vd + j] = self.br.read_bit();
                    }
                    else {
                        result = true;
                        coef_sign[i * vd + j] = false;
                    }
                }
                else {
                    coef_sign[i * vd + j] = false;
                }
            }
        }

        Ok(result)
    }

    /// The real requantization of the MLT coefficients of one subband.
    fn scalar_dequant(
        &mut self,
        index: usize,
        quant_index: i32,
        coef_index: &[i32; SUBBAND_SIZE],
        coef_sign: &[bool; SUBBAND_SIZE],
        mlt: &mut [f32],
    ) {
        let scale = self.rootpow2tab[(quant_index + 63) as usize];

        for i in 0..SUBBAND_SIZE {
            let f1 = if coef_index[i] != 0 {
                let f = QUANT_CENTROID[index][coef_index[i] as usize];
                if coef_sign[i] { -f } else { f }
            }
            else {
                // Noise coding if the coefficient index is 0.
                let f = DITHER[index];
                if self.lfg.next() < 0x8000_0000 { -f } else { f }
            };
            mlt[i] = f1 * scale;
        }
    }

    /// Fills the MLT buffer with MLT coefficients.
    fn decode_vectors(
        &mut self,
        p: &Subpacket,
        category: &mut [i32; 128],
        quant_index_table: &[i32; 102],
        mlt_buffer: &mut [f32],
    ) -> Result<()> {
        // A zero in this table means that the subband coefficient is random noise coded.
        let mut coef_index = [0i32; SUBBAND_SIZE];
        // Whether the subband coefficient is negative.
        let mut coef_sign = [false; SUBBAND_SIZE];

        for band in 0..p.total_subbands {
            let mut index = category[band] as usize;

            if index < 7 && self.unpack_sqvh(p, index, &mut coef_index, &mut coef_sign)? {
                index = 7;
                for j in 0..p.total_subbands {
                    category[band + j] = 7;
                }
            }

            if index >= 7 {
                coef_index = [0; SUBBAND_SIZE];
                coef_sign = [false; SUBBAND_SIZE];
            }

            self.scalar_dequant(
                index,
                quant_index_table[band],
                &coef_index,
                &coef_sign,
                &mut mlt_buffer[band * SUBBAND_SIZE..(band + 1) * SUBBAND_SIZE],
            );
        }

        Ok(())
    }

    fn mono_decode(&mut self, p: &Subpacket, mlt_buffer: &mut [f32]) -> Result<()> {
        let mut category_index = [0i32; 128];
        let mut category = [0i32; 128];
        let mut quant_index_table = [0i32; 102];

        self.decode_envelope(p, &mut quant_index_table)?;

        let num_vectors = self.br.read(p.log2_numvector_size) as usize;

        self.categorize(p, &quant_index_table, &mut category, &mut category_index);

        // Expand the category vector.
        for &idx in category_index.iter().take(num_vectors) {
            let c = &mut category[idx as usize];
            *c += 1;
            if *c as usize >= DITHER.len() {
                *c -= 1;
            }
        }

        if category[..p.total_subbands].iter().any(|&c| c > 7) {
            return decode_error("cook: invalid category");
        }

        self.decode_vectors(p, &mut category, &quant_index_table, mlt_buffer)
    }

    /// Reads the joint-stereo coupling information.
    fn decouple_info(
        &mut self,
        p: &Subpacket,
        decouple_tab: &mut [usize; SUBBAND_SIZE],
    ) -> Result<()> {
        let use_vlc = self.br.read_bit();
        let start = CPLBAND[p.js_subband_start];
        let end = CPLBAND[p.subbands - 1];

        if start > end {
            return Ok(());
        }

        for i in 0..end - start + 1 {
            if use_vlc {
                let vlc = p.channel_coupling.as_ref();
                let Some(vlc) = vlc
                else {
                    return decode_error("cook: missing coupling vlc");
                };
                decouple_tab[start + i] = usize::from(vlc.read(&mut self.br)?);
            }
            else {
                let v = self.br.read(p.js_vlc_bits) as usize;
                if v == (1 << p.js_vlc_bits) - 1 {
                    return decode_error("cook: decouple value too large");
                }
                decouple_tab[start + i] = v;
            }
        }

        Ok(())
    }

    /// Decodes joint-stereo data.
    fn joint_decode(&mut self, p: &Subpacket, bufs: &mut Buffers) -> Result<()> {
        let mut decouple_tab = [0usize; SUBBAND_SIZE];

        bufs.b0.fill(0.0);

        // Make sure the buffers are zeroed out.
        bufs.b1.fill(0.0);
        bufs.b2.fill(0.0);

        self.decouple_info(p, &mut decouple_tab)?;

        self.mono_decode(p, &mut bufs.b0)?;

        // The two channels are stored interleaved in the decode buffer.
        for i in 0..p.js_subband_start {
            for j in 0..SUBBAND_SIZE {
                bufs.b1[i * 20 + j] = bufs.b0[i * 40 + j];
                bufs.b2[i * 20 + j] = bufs.b0[i * 40 + 20 + j];
            }
        }

        // When we reach js_subband_start (the higher frequencies) the coefficients are stored in a
        // coupling scheme.
        let cplscale = CPLSCALES[p.js_vlc_bits as usize - 2];
        let full = (1usize << p.js_vlc_bits) - 1;

        for i in p.js_subband_start..p.subbands {
            let cpl_tmp = decouple_tab[CPLBAND[i]];
            let idx = full - cpl_tmp;
            let f1 = cplscale[cpl_tmp + 1];
            let f2 = cplscale[idx];

            // Decouple a pair of signals from a single signal via multiplication.
            for j in 0..SUBBAND_SIZE {
                let tmp_idx = (p.js_subband_start + i) * SUBBAND_SIZE + j;
                bufs.b1[SUBBAND_SIZE * i + j] = f1 * bufs.b0[tmp_idx];
                bufs.b2[SUBBAND_SIZE * i + j] = f2 * bufs.b0[tmp_idx];
            }
        }

        Ok(())
    }

    /// The modulated lapped transform: transforms MLT coefficients to time-domain samples, applies
    /// the transform window, overlaps the buffers and applies the gain profile.
    fn imlt_gain(
        &mut self,
        coefs: &[f32],
        gains: &Gains,
        previous: &mut [f32],
        out: Option<&mut [f32]>,
    ) {
        let n = self.n;

        // Inverse modified discrete cosine transform. Symphonia's IMDCT has the opposite sign
        // convention to the reference decoder's.
        self.imdct.imdct(&coefs[..n], &mut self.mdct_output);
        for v in self.mdct_output.iter_mut() {
            *v = -*v;
        }

        let (buffer0, buffer1) = self.mdct_output.split_at_mut(n);

        // Apply the window and overlap. The two halves of the time-domain buffer are swapped, and
        // the data saved for the next frame has the wrong sign (hence the subtraction).
        let fc = self.pow2tab[(gains.previous[0] + 63) as usize];
        for i in 0..n {
            buffer1[i] = buffer1[i] * fc * self.window[i] - previous[i] * self.window[n - 1 - i];
        }

        // Apply the gain profile.
        let gsf = self.gain_size_factor;
        for i in 0..8 {
            if gains.now[i] != 0 || gains.now[i + 1] != 0 {
                interpolate(
                    &self.pow2tab,
                    &self.gain_table,
                    &mut buffer1[gsf * i..gsf * (i + 1)],
                    gains.now[i],
                    gains.now[i + 1],
                );
            }
        }

        // Save away the current block to be the previous block.
        previous.copy_from_slice(buffer0);

        // Saturate the output signal.
        if let Some(out) = out {
            for (o, &s) in out.iter_mut().zip(buffer1.iter()) {
                *o = s.clamp(-1.0, 1.0);
            }
        }
    }
}

/// Decoder state: parameters from the extra data, shared DSP state and per-subpacket state.
struct Cook {
    dsp: Dsp,
    bufs: Buffers,
    subpackets: Vec<Subpacket>,
    /// Number of frames decoded so far that only prime the overlap-add state.
    discarded_frames: u32,
    layout: Channels,
}

impl Cook {
    /// Parses the extra data, following FFmpeg's `cook_decode_init`.
    fn try_new(extra: &[u8], channels: usize) -> Result<Self> {
        if extra.len() < 8 {
            return decode_error("cook: necessary extra data missing");
        }

        if channels == 0 {
            return decode_error("cook: invalid number of channels");
        }

        let mut br = ByteReader { data: extra, pos: 0 };

        let mut subpackets: Vec<Subpacket> = Vec::with_capacity(MAX_SUBPACKETS);
        let mut channel_mask = 0u32;
        let mut total_channels = 0;
        // The samples per channel of the first subpacket (the stream-wide value) and of the one
        // being parsed.
        let mut samples_per_channel = 0usize;

        while br.left() > 0 {
            let s = subpackets.len();

            if s >= MAX_SUBPACKETS {
                return unsupported_error("cook: more than 5 subpackets");
            }

            // 8 for mono, 16 for stereo, 20 per subpacket for multichannel.
            let cookversion = br.get::<4>();
            let samples_per_frame = br.get::<2>() as usize;
            let subbands = br.get::<2>() as usize;
            br.get::<4>(); // Unknown, unused.
            let js_subband_start = br.get::<2>() as usize;

            if js_subband_start >= 51 {
                return decode_error("cook: js_subband_start is too large");
            }

            let js_vlc_bits = br.get::<2>();

            let mut sub_spc = samples_per_frame / channels;

            // Default data states.
            let mut log2_numvector_size = 5;
            let mut total_subbands = subbands;
            let mut num_channels = 1;
            let mut bits_per_subpdiv = 0;
            let mut joint_stereo = false;

            // Version-dependent variables.
            match cookversion {
                MONO => {
                    if channels != 1 {
                        return unsupported_error("cook: container channels != 1");
                    }
                }
                STEREO => {
                    if channels != 1 {
                        bits_per_subpdiv = 1;
                        num_channels = 2;
                    }
                }
                JOINT_STEREO => {
                    if channels != 2 {
                        return unsupported_error("cook: container channels != 2");
                    }
                    if extra.len() >= 16 {
                        total_subbands = subbands + js_subband_start;
                        joint_stereo = true;
                        num_channels = 2;
                    }
                    if sub_spc > 256 {
                        log2_numvector_size = 6;
                    }
                    if sub_spc > 512 {
                        log2_numvector_size = 7;
                    }
                }
                MC_COOK => {
                    let mask = br.get::<4>();
                    channel_mask |= mask;

                    if mask.count_ones() > 1 {
                        total_subbands = subbands + js_subband_start;
                        joint_stereo = true;
                        num_channels = 2;
                        sub_spc = samples_per_frame >> 1;

                        if sub_spc > 256 {
                            log2_numvector_size = 6;
                        }
                        if sub_spc > 512 {
                            log2_numvector_size = 7;
                        }
                    }
                    else {
                        sub_spc = samples_per_frame;
                    }
                }
                _ => return unsupported_error("cook: unsupported cook version"),
            }

            if s > 1 && sub_spc != samples_per_channel {
                return decode_error("cook: different number of samples per channel");
            }
            else if s == 0 {
                samples_per_channel = sub_spc;
            }

            // Try to catch some obviously faulty streams.
            if total_subbands > 53 {
                return unsupported_error("cook: total_subbands > 53");
            }

            if joint_stereo && js_subband_start > subbands {
                return decode_error("cook: js_subband_start > subbands");
            }

            if js_vlc_bits > 6 || (js_vlc_bits as usize) < 2 * usize::from(joint_stereo) {
                return decode_error("cook: invalid js_vlc_bits");
            }

            if subbands > 50 {
                return unsupported_error("cook: subbands > 50");
            }

            if subbands == 0 {
                return unsupported_error("cook: subbands = 0");
            }

            if total_channels + num_channels > channels {
                return decode_error("cook: too many subpacket channels");
            }

            let channel_coupling = if joint_stereo {
                let i = js_vlc_bits as usize - 2;
                Some(Vlc::new(&CCPL_HUFFCOUNTS[i], CCPL_HUFFSYMS[i].iter().map(|&s| u16::from(s)))?)
            }
            else {
                None
            };

            subpackets.push(Subpacket {
                ch_idx: 0,
                num_channels,
                subbands,
                js_subband_start,
                js_vlc_bits,
                log2_numvector_size,
                joint_stereo,
                bits_per_subpdiv,
                total_subbands,
                numvector_size: 1 << log2_numvector_size,
                channel_coupling,
                size: 0,
                bits_per_subpacket: 0,
                previous1: Box::default(),
                previous2: Box::default(),
                gains1: Gains::default(),
                gains2: Gains::default(),
            });

            total_channels += num_channels;
        }

        if channel_mask != 0 && channel_mask.count_ones() as usize != total_channels {
            return decode_error("cook: channel mask does not match the subpacket channels");
        }

        // Try to catch some obviously faulty streams.
        if samples_per_channel != 256 && samples_per_channel != 512 && samples_per_channel != 1024 {
            return unsupported_error("cook: unsupported samples per channel");
        }

        let layout = if channel_mask != 0 {
            match Position::from_wave_channel_mask(channel_mask) {
                Some(pos) => Channels::Positioned(pos),
                None => return unsupported_error("cook: unsupported channel mask"),
            }
        }
        else if total_channels != channels {
            return unsupported_error("cook: subpacket channels do not match the stream channels");
        }
        else {
            match channels {
                1 => layouts::CHANNEL_LAYOUT_MONO,
                2 => layouts::CHANNEL_LAYOUT_STEREO,
                _ => match Position::from_wave_channel_count(channels as u32) {
                    Some(pos) => Channels::Positioned(pos),
                    None => return unsupported_error("cook: unsupported channel count"),
                },
            }
        };

        for p in subpackets.iter_mut() {
            p.previous1 = vec![0.0; samples_per_channel].into_boxed_slice();
            p.previous2 = vec![0.0; samples_per_channel].into_boxed_slice();
        }

        let bufs = Buffers {
            b0: vec![0.0; DECODE_BUFFER_LEN].into_boxed_slice(),
            b1: vec![0.0; DECODE_BUFFER_LEN].into_boxed_slice(),
            b2: vec![0.0; DECODE_BUFFER_LEN].into_boxed_slice(),
        };

        Ok(Cook {
            dsp: Dsp::new(samples_per_channel)?,
            bufs,
            subpackets,
            discarded_frames: 0,
            layout,
        })
    }

    /// Clears all signal state so that decoding can restart from an arbitrary frame.
    fn reset(&mut self) {
        for p in self.subpackets.iter_mut() {
            p.previous1.fill(0.0);
            p.previous2.fill(0.0);
            p.gains1 = Gains::default();
            p.gains2 = Gains::default();
        }
        self.dsp.lfg = Lfg::new();
        self.discarded_frames = 0;
    }

    /// Decodes one frame (all the subpackets). The whole of `data` is the frame (the block
    /// alignment). If `out` is `None` only the decoder state is updated.
    fn decode_frame(&mut self, data: &[u8], mut out: Option<&mut AudioBuffer<f32>>) -> Result<()> {
        let block_align = data.len();
        let num_subpackets = self.subpackets.len();

        if block_align < num_subpackets {
            return decode_error("cook: packet is too small");
        }

        // Estimate the subpacket sizes.
        let mut first_size = block_align;
        for i in 1..num_subpackets {
            let size = 2 * usize::from(data[block_align - num_subpackets + i]);
            self.subpackets[i].size = size;
            first_size = match first_size.checked_sub(size + 1) {
                Some(s) => s,
                None => return decode_error("cook: subpacket sizes exceed the packet size"),
            };
        }
        self.subpackets[0].size = first_size;

        // Decode the subpackets.
        let mut offset = 0;
        let mut chidx = 0;

        for p in self.subpackets.iter_mut() {
            p.bits_per_subpacket = (p.size * 8) >> p.bits_per_subpdiv;
            p.ch_idx = chidx;

            decode_subpacket(
                &mut self.dsp,
                &mut self.bufs,
                p,
                &data[offset..],
                out.as_deref_mut(),
            )?;

            offset += p.size;
            chidx += p.num_channels;
        }

        if self.discarded_frames < DISCARDED_FRAMES {
            self.discarded_frames += 1;
        }

        Ok(())
    }
}

/// Gets the output plane of channel `ch`, if output is enabled.
fn out_plane<'a>(
    out: &'a mut Option<&mut AudioBuffer<f32>>,
    ch: usize,
) -> Result<Option<&'a mut [f32]>> {
    match out.as_deref_mut() {
        Some(buf) => match buf.plane_mut(ch) {
            Some(plane) => Ok(Some(plane)),
            None => decode_error("cook: missing output channel"),
        },
        None => Ok(None),
    }
}

/// Decodes one subpacket and writes the PCM of its channels (if `out` is provided).
fn decode_subpacket(
    dsp: &mut Dsp,
    bufs: &mut Buffers,
    p: &mut Subpacket,
    data: &[u8],
    mut out: Option<&mut AudioBuffer<f32>>,
) -> Result<()> {
    bufs.b1.fill(0.0);

    let mut gains1 = p.gains1;
    dsp.decode_bytes_and_gain(p, data, &mut gains1);

    let mut gains2 = p.gains2;

    if p.joint_stereo {
        dsp.joint_decode(p, bufs)?;
    }
    else {
        dsp.mono_decode(p, &mut bufs.b1)?;

        if p.num_channels == 2 {
            bufs.b2.fill(0.0);
            dsp.decode_bytes_and_gain(p, data.get(p.size / 2..).unwrap_or(&[]), &mut gains2);
            dsp.mono_decode(p, &mut bufs.b2)?;
        }
    }

    p.gains1 = gains1;
    p.gains2 = gains2;

    dsp.imlt_gain(&bufs.b1, &p.gains1, &mut p.previous1, out_plane(&mut out, p.ch_idx)?);

    if p.num_channels == 2 {
        // Joint-stereo channels share one set of gains.
        let gains = if p.joint_stereo { &p.gains1 } else { &p.gains2 };
        dsp.imlt_gain(&bufs.b2, gains, &mut p.previous2, out_plane(&mut out, p.ch_idx + 1)?);
    }

    Ok(())
}

/// Cook decoder.
pub struct CookDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    cook: Cook,
}

impl CookDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_COOK {
            return unsupported_error("cook: invalid codec");
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("cook: sample rate is required"),
        };

        let channels = match &params.channels {
            Some(channels) if channels.count() > 0 => channels.count(),
            _ => return unsupported_error("cook: channels are required"),
        };

        let extra = match &params.extra_data {
            Some(extra) => extra,
            None => return unsupported_error("cook: extra data is required"),
        };

        let cook = Cook::try_new(extra, channels)?;

        let spec = AudioSpec::new(rate, cook.layout.clone());
        let buf = AudioBuffer::new(spec, cook.dsp.n);

        let mut params = params.clone();
        params.with_channels(cook.layout.clone());

        Ok(CookDecoder { params, opts: *opts, buf, cook })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        let output = self.cook.discarded_frames >= DISCARDED_FRAMES;

        if output {
            self.buf.render_uninit(Some(self.cook.dsp.n));
        }

        self.cook.decode_frame(packet.data, if output { Some(&mut self.buf) } else { None })?;

        if output && self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for CookDecoder {
    fn reset(&mut self) {
        self.cook.reset();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
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

impl RegisterableAudioDecoder for CookDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(CookDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_COOK, "cook", "Cook / Cooker / Gecko (RealAudio G2)")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Audio;

    const MONO_EXTRA: [u8; 8] = [0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x25];
    const JOINT_EXTRA: [u8; 16] =
        [0x01, 0x00, 0x00, 0x03, 0x08, 0x00, 0x00, 0x20, 0, 0, 0, 0, 0x00, 0x02, 0x00, 0x04];
    /// 5.1 (FL+FR, FC, LFE, BL+BR subpackets), from `Surround_6ch.rma`.
    const MC_EXTRA_HEX: [&str; 4] = [
        "0200000008000025000000000002000400000003",
        "020000000400002f000000000000000000000004",
        "0200000004000001000000000000000000000008",
        "0200000008000020000000000002000400000030",
    ];

    fn mc_extra() -> Vec<u8> {
        let hex: String = MC_EXTRA_HEX.concat();
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn decoder(extra: &[u8], channels: usize, rate: u32) -> Result<CookDecoder> {
        let layout = match channels {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            n => Channels::Positioned(Position::from_wave_channel_count(n as u32).unwrap()),
        };
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_COOK)
            .with_sample_rate(rate)
            .with_channels(layout)
            .with_extra_data(extra.to_vec().into_boxed_slice());
        CookDecoder::try_new(&p, &AudioDecoderOptions::default())
    }

    #[test]
    fn verify_vlc_tables_are_complete() {
        for i in 0..13 {
            let v = Vlc::new(
                &ENVELOPE_QUANT_INDEX_HUFFCOUNTS[i],
                ENVELOPE_QUANT_INDEX_HUFFSYMS[i].iter().map(|&s| u16::from(s)),
            )
            .unwrap();
            assert_eq!(v.limit[16], 1 << 16, "envelope table {i}");
        }
        for i in 0..7 {
            let v = Vlc::new(&CVH_HUFFCOUNTS[i], CVH_HUFFSYMS[i].iter().copied()).unwrap();
            assert_eq!(v.limit[16], 1 << 16, "sqvh table {i}");
        }
        for i in 0..5 {
            let v = Vlc::new(&CCPL_HUFFCOUNTS[i], CCPL_HUFFSYMS[i].iter().map(|&s| u16::from(s)))
                .unwrap();
            assert_eq!(v.limit[16], 1 << 16, "coupling table {i}");
        }
    }

    #[test]
    fn verify_missing_or_short_extra_data_is_rejected() {
        assert!(decoder(&[1, 0, 0, 1], 1, 44100).is_err());

        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_COOK)
            .with_sample_rate(44100)
            .with_channels(layouts::CHANNEL_LAYOUT_MONO);
        assert!(CookDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }

    #[test]
    fn verify_channel_mismatch_is_rejected() {
        // Mono stream data in a stereo container.
        assert!(decoder(&MONO_EXTRA, 2, 44100).is_err());
        // Joint-stereo data in a mono container.
        assert!(decoder(&JOINT_EXTRA, 1, 44100).is_err());
    }

    #[test]
    fn verify_unknown_version_is_rejected() {
        let mut extra = MONO_EXTRA;
        extra[3] = 0x09;
        assert!(decoder(&extra, 1, 44100).is_err());
    }

    #[test]
    fn verify_multichannel_layout() {
        let dec = decoder(&mc_extra(), 6, 44100).unwrap();
        assert_eq!(dec.cook.subpackets.len(), 4);
        assert_eq!(dec.params.channels.as_ref().map(|c| c.count()), Some(6));
    }

    #[test]
    fn verify_empty_packet_is_error() {
        let mut dec = decoder(&MONO_EXTRA, 1, 44100).unwrap();
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &[]);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    #[test]
    fn verify_first_frames_are_discarded() {
        let mut dec = decoder(&MONO_EXTRA, 1, 44100).unwrap();
        let data = [0u8; 93];
        for i in 0..4 {
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let frames = dec.decode_ref(&pkt).unwrap().frames();
            assert_eq!(frames, if i < 2 { 0 } else { 1024 });
        }
        dec.reset();
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert_eq!(dec.decode_ref(&pkt).unwrap().frames(), 0);
    }

    /// Arbitrary (corrupt) input must never panic and must always yield finite output.
    #[test]
    fn verify_random_packets_do_not_panic() {
        let mc = mc_extra();
        let cases: [(&[u8], usize, usize); 3] =
            [(&MONO_EXTRA, 1, 93), (&JOINT_EXTRA, 2, 93), (&mc, 6, 381)];

        let mut state = 0x1234_5678_9abc_def0u64;

        for (extra, channels, size) in cases {
            let mut dec = decoder(extra, channels, 44100).unwrap();
            let mut data = vec![0u8; size];
            for _ in 0..300 {
                for b in data.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state >> 32) as u8;
                }
                let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
                if let Ok(GenericAudioBufferRef::F32(b)) = dec.decode_ref(&pkt) {
                    for ch in 0..b.num_planes() {
                        assert!(b.plane(ch).unwrap().iter().all(|s| s.is_finite()));
                    }
                }
            }
        }
    }
}
