// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// WavPack v4/v5 lossless PCM decoder.
// Ported from the WavPack reference implementation by Conifer Software
// (https://github.com/dbry/WavPack, BSD licence).

// ---------------------------------------------------------------------------
// Block flag bits (wavpack.h)
// ---------------------------------------------------------------------------

pub const MONO_FLAG:      u32 = 0x0000_0004;
pub const HYBRID_FLAG:    u32 = 0x0000_0008;
pub const JOINT_STEREO:   u32 = 0x0000_0010;
pub const CROSS_DECORR:   u32 = 0x0000_0020;
pub const HYBRID_SHAPE:   u32 = 0x0000_0040;
pub const FLOAT_DATA:     u32 = 0x0000_0080;
pub const INT32_DATA:     u32 = 0x0000_0100;
pub const HYBRID_BITRATE: u32 = 0x0000_0200;
pub const HYBRID_BALANCE: u32 = 0x0000_0400;
pub const SHIFT_LSB:      u32 = 13;
pub const SHIFT_MASK:     u32 = 0x1f << 13;
pub const MAG_LSB:        u32 = 18;
pub const MAG_MASK:       u32 = 0x1f << 18;
pub const NEW_SHAPING:    u32 = 0x2000_0000;
pub const FALSE_STEREO:   u32 = 0x4000_0000;
pub const MONO_DATA:      u32 = MONO_FLAG | FALSE_STEREO;

// ---------------------------------------------------------------------------
// Packet magic — 4 bytes that distinguish a v4/v5 packet from a v3 prefix
// ---------------------------------------------------------------------------

pub const PACKET_MAGIC: &[u8; 4] = b"WV45";

// Header size of one stream's mini-block within a (possibly multi-stream) packet, not
// counting the packet-level magic/stream-count or this stream's own length prefix
// (both added by the reader when assembling the packet; see
// `WavPackReader::read_v4v5_stream_block`):
//   flags(4) + block_samples(4) + crc(4)
//   + terms_len(4) + weights_len(4) + samples_len(4) + entropy_len(4)
//   + hybrid_profile_len(4) + float_info_len(4) + int32_len(4) + wvx_len(4)
//   + shaping_len(4) + wvc_len(4) + wvc_wvx_len(4) + wvc_crc(4) + ext_flags(4) = 64 bytes
//
// `shaping`, `wvc` and `wvc_wvx` come from the matching block of the `.wvc` correction file
// (all empty without one). `ext_flags` bit 0: `wvx` is an `ID_WVX_NEW_BITSTREAM`; bit 1:
// `wvc_wvx` is.
pub const STREAM_HDR: usize = 64;

/// `ext_flags` bit: the main block's extension bitstream is the "new" kind.
pub const EXT_WVX_NEW: u32 = 1 << 0;
/// `ext_flags` bit: the correction block's extension bitstream is the "new" kind.
pub const EXT_WVC_WVX_NEW: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// Sub-block size limits to guard against malformed streams
// ---------------------------------------------------------------------------

const MAX_NTERMS: usize = 16;
const MAX_TERM:   usize = 8;
const LIMIT_ONES: u32   = 16;

/// WavPack's own encoder caps block size at 131072 samples (`--blocksize`); this is a
/// generous multiple of that used to reject a malformed/mutated `block_samples` field
/// before it can cause an arithmetic overflow or an unbounded allocation.
const MAX_BLOCK_SAMPLES: u32 = 1 << 20;

// INC/DEC median divisors
const DIV0: u32 = 128;
const DIV1: u32 = 64;
const DIV2: u32 = 32;

// Lightweight trace toggle: set WAVPACK_TRACE=1 to enable stderr debug prints.
fn trace_enabled() -> bool {
    use std::sync::atomic::{AtomicI8, Ordering};
    static FLAG: AtomicI8 = AtomicI8::new(-1);
    match FLAG.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let v = match std::env::var("WAVPACK_TRACE").ok().as_deref() {
                Some("1") | Some("true") | Some("yes") => 1,
                _ => 0,
            };
            FLAG.store(v, Ordering::Relaxed);
            v == 1
        }
    }
}

macro_rules! wp_trace {
    ($($arg:tt)*) => { if $crate::decoder::v4v5::trace_enabled() { eprintln!($($arg)*); } };
}

// ---------------------------------------------------------------------------
// LSB-first bit reader (identical scheme to the v3 Bits struct)
// ---------------------------------------------------------------------------

/// A stream that reads past its end yields zero bits, like the C reference. The window `sr`
/// holds the next `bc` valid bits of the stream (LSB first), and is refilled a whole `u64` at a
/// time; bits above `bc` are either zero or the genuine following bits of the stream.
pub(super) struct Bits<'a> {
    data: &'a [u8],
    ptr:  usize,
    bc:   u32,
    sr:   u64,
}

impl<'a> Bits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Bits { data, ptr: 0, bc: 0, sr: 0 }
    }

    /// Top the window up to at least 56 valid bits.
    #[inline(always)]
    fn refill(&mut self) {
        match self.data.get(self.ptr..).and_then(|rest| rest.first_chunk::<8>()) {
            Some(chunk) => {
                self.sr |= u64::from_le_bytes(*chunk) << self.bc;
                self.ptr += ((63 - self.bc) >> 3) as usize;
                self.bc |= 56;
            }
            None => self.refill_slow(),
        }
    }

    /// Refill near the end of the data: byte by byte, padding with zeros.
    #[cold]
    #[inline(never)]
    fn refill_slow(&mut self) {
        while self.bc <= 56 {
            let byte = match self.data.get(self.ptr) {
                Some(&b) => {
                    self.ptr += 1;
                    b
                }
                None => 0,
            };
            self.sr |= u64::from(byte) << self.bc;
            self.bc += 8;
        }
    }

    #[inline(always)]
    pub(super) fn getbit(&mut self) -> u32 {
        if self.bc == 0 {
            self.refill();
        }
        let bit = (self.sr & 1) as u32;
        self.sr >>= 1;
        self.bc -= 1;
        bit
    }

    /// Read `nbits` (at most 32) bits, LSB first.
    #[inline(always)]
    pub(super) fn getbits(&mut self, nbits: u32) -> u32 {
        debug_assert!(nbits <= 32);
        if nbits == 0 {
            return 0;
        }
        if self.bc < nbits {
            self.refill();
        }
        let val = (self.sr & ((1u64 << nbits) - 1)) as u32;
        self.sr >>= nbits;
        self.bc -= nbits;
        val
    }

    /// Count (and consume) the 1 bits up to `max` (at most 33), and, if fewer than `max` were
    /// found, the 0 bit that ends them. Equivalent to `while n < max && getbit() == 1 { n += 1 }`.
    #[inline(always)]
    fn count_ones(&mut self, max: u32) -> u32 {
        debug_assert!(max <= 33);
        if self.bc < 40 {
            self.refill();
        }
        let ones = (!self.sr).trailing_zeros();
        let n = if ones >= max { max } else { ones + 1 };
        self.sr >>= n;
        self.bc -= n;
        ones.min(max)
    }
}

// ---------------------------------------------------------------------------
// count_bits: number of bits needed to represent n
// ---------------------------------------------------------------------------

#[inline(always)]
fn count_bits(n: u32) -> u32 {
    32 - n.leading_zeros()
}

// ---------------------------------------------------------------------------
// read_code: read a range-coded value in [0, maxcode]
// Portable version of the WavPack read_code() function.
// ---------------------------------------------------------------------------

fn read_code(bs: &mut Bits<'_>, maxcode: u32) -> u32 {
    if maxcode == 0 { return 0; }
    if maxcode == 1 { return bs.getbit(); }

    let bitcount = count_bits(maxcode);
    let extras    = (1u32 << bitcount) - maxcode - 1;
    let code      = bs.getbits(bitcount - 1);

    if code >= extras {
        (code << 1) - extras + bs.getbit()
    } else {
        code
    }
}

// ---------------------------------------------------------------------------
// wp_exp2s: decode log2 representation stored in sub-blocks.
// Ported from entropy_utils.c; uses the same exp2_table lookup.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
static EXP2_TABLE: [u8; 256] = [
    0x00,0x01,0x01,0x02,0x03,0x03,0x04,0x05,0x06,0x06,0x07,0x08,0x08,0x09,0x0a,0x0b,
    0x0b,0x0c,0x0d,0x0e,0x0e,0x0f,0x10,0x10,0x11,0x12,0x13,0x13,0x14,0x15,0x16,0x16,
    0x17,0x18,0x19,0x19,0x1a,0x1b,0x1c,0x1d,0x1d,0x1e,0x1f,0x20,0x20,0x21,0x22,0x23,
    0x24,0x24,0x25,0x26,0x27,0x28,0x28,0x29,0x2a,0x2b,0x2c,0x2c,0x2d,0x2e,0x2f,0x30,
    0x30,0x31,0x32,0x33,0x34,0x35,0x35,0x36,0x37,0x38,0x39,0x3a,0x3a,0x3b,0x3c,0x3d,
    0x3e,0x3f,0x40,0x41,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x48,0x49,0x4a,0x4b,
    0x4c,0x4d,0x4e,0x4f,0x50,0x51,0x51,0x52,0x53,0x54,0x55,0x56,0x57,0x58,0x59,0x5a,
    0x5b,0x5c,0x5d,0x5e,0x5e,0x5f,0x60,0x61,0x62,0x63,0x64,0x65,0x66,0x67,0x68,0x69,
    0x6a,0x6b,0x6c,0x6d,0x6e,0x6f,0x70,0x71,0x72,0x73,0x74,0x75,0x76,0x77,0x78,0x79,
    0x7a,0x7b,0x7c,0x7d,0x7e,0x7f,0x80,0x81,0x82,0x83,0x84,0x85,0x87,0x88,0x89,0x8a,
    0x8b,0x8c,0x8d,0x8e,0x8f,0x90,0x91,0x92,0x93,0x95,0x96,0x97,0x98,0x99,0x9a,0x9b,
    0x9c,0x9d,0x9f,0xa0,0xa1,0xa2,0xa3,0xa4,0xa5,0xa6,0xa8,0xa9,0xaa,0xab,0xac,0xad,
    0xaf,0xb0,0xb1,0xb2,0xb3,0xb4,0xb6,0xb7,0xb8,0xb9,0xba,0xbc,0xbd,0xbe,0xbf,0xc0,
    0xc2,0xc3,0xc4,0xc5,0xc6,0xc8,0xc9,0xca,0xcb,0xcd,0xce,0xcf,0xd0,0xd2,0xd3,0xd4,
    0xd6,0xd7,0xd8,0xd9,0xdb,0xdc,0xdd,0xde,0xe0,0xe1,0xe2,0xe4,0xe5,0xe6,0xe8,0xe9,
    0xea,0xec,0xed,0xee,0xf0,0xf1,0xf2,0xf4,0xf5,0xf6,0xf8,0xf9,0xfa,0xfc,0xfd,0xff,
];

fn wp_exp2s(log: i32) -> i32 {
    if log < 0 {
        return -(wp_exp2s(-log));
    }
    let value = (EXP2_TABLE[(log & 0xff) as usize] as u32) | 0x100;
    let shift  = log >> 8;
    if shift <= 9 {
        (value >> (9 - shift)) as i32
    } else {
        (value << ((shift - 9) & 0x1f)) as i32
    }
}

// ---------------------------------------------------------------------------
// Decorrelation pass
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct DecorrPass {
    pub term:      i32,
    pub delta:     i32,
    pub weight_a:  i32,
    pub weight_b:  i32,
    pub samples_a: [i32; MAX_TERM],
    pub samples_b: [i32; MAX_TERM],
}

// ---------------------------------------------------------------------------
// Entropy / words state
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
pub struct EntropyChannel {
    pub median: [u32; 3],
    /// Hybrid mode only: running estimate of the residual magnitude, used to derive
    /// `error_limit`. Ported from `entropy_data.slow_level` (wavpack_local.h).
    pub slow_level: u32,
    /// Hybrid mode only: half-width of the "uncertain" range the main bitstream still
    /// has to narrow down (0 means lossless / exact for this channel).
    pub error_limit: u32,
}

#[derive(Default, Clone)]
pub struct WordsState {
    pub c:            [EntropyChannel; 2],
    pub holding_one:  u32,
    pub holding_zero: i32,
    pub zeros_acc:    u32,
    /// Hybrid mode only: `ID_HYBRID_PROFILE` bitrate accumulator/delta (wavpack_local.h
    /// `words_data.bitrate_acc` / `bitrate_delta`), used by `update_error_limit`.
    pub bitrate_acc:   [u32; 2],
    pub bitrate_delta: [u32; 2],
}

// ---------------------------------------------------------------------------
// Int32 info (from ID_INT32_INFO sub-block)
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
pub struct Int32Info {
    pub sent_bits: u8,
    pub zeros:     u8,
    pub ones:      u8,
    pub dups:      u8,
}

// ---------------------------------------------------------------------------
// Sub-block parsers — called from decoder with raw sub-block bytes
// ---------------------------------------------------------------------------

fn restore_weight(b: i8) -> i32 {
    let r = (b as i32) << 3;
    if r > 0 { r + ((r + 64) >> 7) } else { r }
}

pub fn parse_decorr_terms(data: &[u8], passes: &mut Vec<DecorrPass>) {
    // ID_DECORR_TERMS stores terms in REVERSE order relative to dpp[]:
    // bytes are written by the encoder iterating dpp from last to first.
    // Mirror the C reference (decorr_utils.c read_decorr_terms) and place
    // byte[0] into passes[num-1], byte[num-1] into passes[0].
    let num = data.len().min(MAX_NTERMS);
    passes.clear();
    passes.resize_with(num, DecorrPass::default);
    for i in 0..num {
        let byte  = data[i];
        let term  = ((byte & 0x1f) as i32) - 5;
        let delta = ((byte >> 5) & 0x7) as i32;
        passes[num - 1 - i].term  = term;
        passes[num - 1 - i].delta = delta;
    }
}

pub fn parse_decorr_weights(data: &[u8], passes: &mut [DecorrPass], is_mono: bool) {
    // ID_DECORR_WEIGHTS also stores entries in REVERSE dpp[] order — see
    // decorr_utils.c read_decorr_weights. Walk bytes forward, fill passes
    // from the last index backwards. Unused weights (no byte) stay at 0
    // (already cleared via Default).
    let stride = if is_mono { 1usize } else { 2 };
    let n = passes.len();
    let mut byte = 0usize;
    for j in 0..n {
        let p = &mut passes[n - 1 - j];
        if byte + stride > data.len() {
            break;
        }
        p.weight_a = restore_weight(data[byte] as i8);
        if !is_mono {
            p.weight_b = restore_weight(data[byte + 1] as i8);
        }
        byte += stride;
    }
}

pub fn parse_decorr_samples(data: &[u8], passes: &mut [DecorrPass], is_mono: bool) {
    // ID_DECORR_SAMPLES also iterates dpp in REVERSE — see
    // decorr_utils.c read_decorr_samples. Mirror that order here.
    let mut ptr = 0usize;

    let read_i16 = |data: &[u8], ptr: &mut usize| -> i32 {
        let v = if *ptr + 2 <= data.len() {
            i16::from_le_bytes([data[*ptr], data[*ptr + 1]]) as i32
        } else {
            0
        };
        *ptr += 2;
        v
    };

    let n = passes.len();
    for j in 0..n {
        let p = &mut passes[n - 1 - j];
        p.samples_a = [0i32; MAX_TERM];
        p.samples_b = [0i32; MAX_TERM];

        if p.term > MAX_TERM as i32 {
            // terms 17 and 18: linear/quadratic extrapolation from 2 samples each
            p.samples_a[0] = wp_exp2s(read_i16(data, &mut ptr));
            p.samples_a[1] = wp_exp2s(read_i16(data, &mut ptr));
            if !is_mono {
                p.samples_b[0] = wp_exp2s(read_i16(data, &mut ptr));
                p.samples_b[1] = wp_exp2s(read_i16(data, &mut ptr));
            }
        } else if p.term < 0 {
            p.samples_a[0] = wp_exp2s(read_i16(data, &mut ptr));
            p.samples_b[0] = wp_exp2s(read_i16(data, &mut ptr));
        } else {
            let cnt = p.term as usize;
            for m in 0..cnt {
                p.samples_a[m] = wp_exp2s(read_i16(data, &mut ptr));
                if !is_mono {
                    p.samples_b[m] = wp_exp2s(read_i16(data, &mut ptr));
                }
            }
        }

        if ptr > data.len() { break; }
    }
}

pub fn parse_entropy_vars(data: &[u8], ws: &mut WordsState, is_mono: bool) {
    wp_trace!("[pev] entropy_raw ({} bytes): {:02x?}", data.len(), data);
    let mut ptr = 0usize;
    let read_i16 = |data: &[u8], ptr: &mut usize| -> i32 {
        let v = if *ptr + 2 <= data.len() {
            i16::from_le_bytes([data[*ptr], data[*ptr + 1]]) as i32
        } else {
            0
        };
        *ptr += 2;
        v
    };
    let log0 = read_i16(data, &mut ptr);
    let log1 = read_i16(data, &mut ptr);
    let log2 = read_i16(data, &mut ptr);
    ws.c[0].median[0] = wp_exp2s(log0) as u32;
    ws.c[0].median[1] = wp_exp2s(log1) as u32;
    ws.c[0].median[2] = wp_exp2s(log2) as u32;
    wp_trace!("[pev] ch0 logs={},{},{} medians={},{},{}", log0,log1,log2, ws.c[0].median[0],ws.c[0].median[1],ws.c[0].median[2]);
    if !is_mono {
        let log0b = read_i16(data, &mut ptr);
        let log1b = read_i16(data, &mut ptr);
        let log2b = read_i16(data, &mut ptr);
        ws.c[1].median[0] = wp_exp2s(log0b) as u32;
        ws.c[1].median[1] = wp_exp2s(log1b) as u32;
        ws.c[1].median[2] = wp_exp2s(log2b) as u32;
        wp_trace!("[pev] ch1 logs={},{},{} medians={},{},{}", log0b,log1b,log2b, ws.c[1].median[0],ws.c[1].median[1],ws.c[1].median[2]);
    }
}

pub fn parse_int32_info(data: &[u8]) -> Int32Info {
    if data.len() < 4 { return Int32Info::default(); }
    Int32Info { sent_bits: data[0], zeros: data[1], ones: data[2], dups: data[3] }
}

// ---------------------------------------------------------------------------
// wp_log2 / hybrid profile parsing (entropy_utils.c: read_hybrid_profile,
// update_error_limit, wp_log2). Only used for hybrid (lossy or hybrid-lossless)
// streams; ordinary lossless streams never carry an ID_HYBRID_PROFILE sub-block.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
static NBITS_TABLE: [u8; 256] = [
    0, 1, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
];

#[rustfmt::skip]
static LOG2_TABLE: [u8; 256] = [
    0x00, 0x01, 0x03, 0x04, 0x06, 0x07, 0x09, 0x0a, 0x0b, 0x0d, 0x0e, 0x10, 0x11, 0x12, 0x14, 0x15,
    0x16, 0x18, 0x19, 0x1a, 0x1c, 0x1d, 0x1e, 0x20, 0x21, 0x22, 0x24, 0x25, 0x26, 0x28, 0x29, 0x2a,
    0x2c, 0x2d, 0x2e, 0x2f, 0x31, 0x32, 0x33, 0x34, 0x36, 0x37, 0x38, 0x39, 0x3b, 0x3c, 0x3d, 0x3e,
    0x3f, 0x41, 0x42, 0x43, 0x44, 0x45, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4d, 0x4e, 0x4f, 0x50, 0x51,
    0x52, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63,
    0x64, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85,
    0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95,
    0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4,
    0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb2,
    0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xc0,
    0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcb, 0xcc, 0xcd, 0xce,
    0xcf, 0xd0, 0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd8, 0xd9, 0xda, 0xdb,
    0xdc, 0xdc, 0xdd, 0xde, 0xdf, 0xe0, 0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe4, 0xe5, 0xe6, 0xe7, 0xe7,
    0xe8, 0xe9, 0xea, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xee, 0xef, 0xf0, 0xf1, 0xf1, 0xf2, 0xf3, 0xf4,
    0xf4, 0xf5, 0xf6, 0xf7, 0xf7, 0xf8, 0xf9, 0xf9, 0xfa, 0xfb, 0xfc, 0xfc, 0xfd, 0xfe, 0xff, 0xff,
];

/// Port of `wp_log2()` in entropy_utils.c.
fn wp_log2(avalue: u32) -> i32 {
    let avalue = avalue.wrapping_add(avalue >> 9);
    if avalue < (1 << 8) {
        let dbits = NBITS_TABLE[avalue as usize] as u32;
        ((dbits << 8) + LOG2_TABLE[((avalue << (9 - dbits)) & 0xff) as usize] as u32) as i32
    } else {
        let dbits = if avalue < (1 << 16) {
            NBITS_TABLE[(avalue >> 8) as usize] as u32 + 8
        } else if avalue < (1 << 24) {
            NBITS_TABLE[(avalue >> 16) as usize] as u32 + 16
        } else {
            NBITS_TABLE[(avalue >> 24) as usize] as u32 + 24
        };
        ((dbits << 8) + LOG2_TABLE[((avalue >> (dbits - 9)) & 0xff) as usize] as u32) as i32
    }
}

/// Read the contents of an `ID_HYBRID_PROFILE` sub-block into `ws`.
/// Port of `read_hybrid_profile()` in entropy_utils.c.
pub fn parse_hybrid_profile(data: &[u8], ws: &mut WordsState, is_mono: bool, flags: u32) {
    let mut ptr = 0usize;
    let read_u16 = |data: &[u8], ptr: &mut usize| -> u32 {
        let v = if *ptr + 2 <= data.len() {
            data[*ptr] as u32 | ((data[*ptr + 1] as u32) << 8)
        } else {
            0
        };
        *ptr += 2;
        v
    };

    if (flags & HYBRID_BITRATE) != 0 {
        // Not sign-extended: slow_level is always a non-negative log2 magnitude.
        ws.c[0].slow_level = wp_exp2s(read_u16(data, &mut ptr) as i32) as u32;
        if !is_mono {
            ws.c[1].slow_level = wp_exp2s(read_u16(data, &mut ptr) as i32) as u32;
        }
    }

    ws.bitrate_acc[0] = read_u16(data, &mut ptr) << 16;
    if !is_mono {
        ws.bitrate_acc[1] = read_u16(data, &mut ptr) << 16;
    }

    if ptr < data.len() {
        // Sign-extended: the delta can move the bitrate up or down over time.
        ws.bitrate_delta[0] = wp_exp2s(read_u16(data, &mut ptr) as i16 as i32) as u32;
        if !is_mono {
            ws.bitrate_delta[1] = wp_exp2s(read_u16(data, &mut ptr) as i16 as i32) as u32;
        }
    } else {
        ws.bitrate_delta[0] = 0;
        ws.bitrate_delta[1] = 0;
    }
    wp_trace!("[phyb] flags={:#x} bitrate_bits={} data_len={} bitrate_acc={:?} bitrate_delta={:?} slow_level={:?}",
        flags, flags & HYBRID_BITRATE, data.len(), ws.bitrate_acc, ws.bitrate_delta,
        [ws.c[0].slow_level, ws.c[1].slow_level]);
}

/// Port of `update_error_limit()` in entropy_utils.c. Called once per sample (for
/// channel 0 only, per the reference) while decoding a hybrid block.
fn update_error_limit(ws: &mut WordsState, flags: u32) {
    let is_mono = (flags & MONO_DATA) != 0;
    let hybrid_bitrate = (flags & HYBRID_BITRATE) != 0;

    ws.bitrate_acc[0] = ws.bitrate_acc[0].wrapping_add(ws.bitrate_delta[0]);
    let bitrate_0 = (ws.bitrate_acc[0] >> 16) as i32;

    if is_mono {
        ws.c[0].error_limit = if hybrid_bitrate {
            let slow_log_0 = ((ws.c[0].slow_level.wrapping_add(SLO)) >> SLS) as i32;
            if slow_log_0 - bitrate_0 > -0x100 {
                wp_exp2s(slow_log_0 - bitrate_0 + 0x100) as u32
            } else {
                0
            }
        } else {
            wp_exp2s(bitrate_0) as u32
        };
    } else {
        ws.bitrate_acc[1] = ws.bitrate_acc[1].wrapping_add(ws.bitrate_delta[1]);
        let mut bitrate_1 = (ws.bitrate_acc[1] >> 16) as i32;
        let mut bitrate_0 = bitrate_0;

        if hybrid_bitrate {
            let slow_log_0 = ((ws.c[0].slow_level.wrapping_add(SLO)) >> SLS) as i32;
            let slow_log_1 = ((ws.c[1].slow_level.wrapping_add(SLO)) >> SLS) as i32;

            if (flags & HYBRID_BALANCE) != 0 {
                let balance = (slow_log_1 - slow_log_0 + bitrate_1 + 1) >> 1;
                if balance > bitrate_0 {
                    bitrate_1 = bitrate_0 * 2;
                    bitrate_0 = 0;
                } else if -balance > bitrate_0 {
                    bitrate_0 *= 2;
                    bitrate_1 = 0;
                } else {
                    bitrate_1 = bitrate_0 + balance;
                    bitrate_0 -= balance;
                }
            }

            ws.c[0].error_limit = if slow_log_0 - bitrate_0 > -0x100 {
                wp_exp2s(slow_log_0 - bitrate_0 + 0x100) as u32
            } else {
                0
            };
            ws.c[1].error_limit = if slow_log_1 - bitrate_1 > -0x100 {
                wp_exp2s(slow_log_1 - bitrate_1 + 0x100) as u32
            } else {
                0
            };
        } else {
            ws.c[0].error_limit = wp_exp2s(bitrate_0) as u32;
            ws.c[1].error_limit = wp_exp2s(bitrate_1) as u32;
        }
    }
}

// ---------------------------------------------------------------------------
// Median helpers (INC/DEC/GET from wavpack_local.h)
// ---------------------------------------------------------------------------

#[inline(always)]
fn get_med(med: u32) -> u32 { (med >> 4) + 1 }

// Median update helpers: use wrapping arithmetic to match C's uint32_t behaviour.

#[inline(always)]
fn inc_med0(c: &mut EntropyChannel) {
    c.median[0] = c.median[0].wrapping_add(
        c.median[0].wrapping_add(DIV0) / DIV0 * 5,
    );
}
#[inline(always)]
fn dec_med0(c: &mut EntropyChannel) {
    c.median[0] = c.median[0].wrapping_sub(
        (c.median[0].wrapping_add(DIV0 - 2)) / DIV0 * 2,
    );
}
#[inline(always)]
fn inc_med1(c: &mut EntropyChannel) {
    c.median[1] = c.median[1].wrapping_add(
        c.median[1].wrapping_add(DIV1) / DIV1 * 5,
    );
}
#[inline(always)]
fn dec_med1(c: &mut EntropyChannel) {
    c.median[1] = c.median[1].wrapping_sub(
        (c.median[1].wrapping_add(DIV1 - 2)) / DIV1 * 2,
    );
}
#[inline(always)]
fn inc_med2(c: &mut EntropyChannel) {
    c.median[2] = c.median[2].wrapping_add(
        c.median[2].wrapping_add(DIV2) / DIV2 * 5,
    );
}
#[inline(always)]
fn dec_med2(c: &mut EntropyChannel) {
    c.median[2] = c.median[2].wrapping_sub(
        (c.median[2].wrapping_add(DIV2 - 2)) / DIV2 * 2,
    );
}

// ---------------------------------------------------------------------------
// count_ones: read 1-bits until 0 (or LIMIT_ONES), with extended range code
// Returns None on end-of-stream (33 ones or 33 cbits seen)
// ---------------------------------------------------------------------------

/// Read an extended-range count (the "zero run" length, or the large part of a ones count):
/// a unary number of bits `cbits` followed by `cbits - 1` bits. `None` is the end of the stream
/// (33 ones).
#[inline(always)]
fn read_ext_count(bs: &mut Bits<'_>) -> Option<u32> {
    let cbits = bs.count_ones(33);
    if cbits == 33 {
        return None;
    }
    if cbits < 2 {
        Some(cbits)
    } else {
        Some(bs.getbits(cbits - 1) | (1u32 << (cbits - 1)))
    }
}

fn count_ones_lim(bs: &mut Bits<'_>) -> Option<u32> {
    let ones_count = bs.count_ones(LIMIT_ONES + 1);

    if ones_count >= LIMIT_ONES {
        if ones_count > LIMIT_ONES { return None; } // 17+ consecutive ones = EOS

        // Extended range coding for large values
        return Some(read_ext_count(bs)?.wrapping_add(LIMIT_ONES));
    }

    Some(ones_count)
}

// ---------------------------------------------------------------------------
// get_words_lossless: entropy decode block_samples frames into buffer
// Returns interleaved samples: stereo → L0,R0,L1,R1,…; mono → S0,S1,…
// ---------------------------------------------------------------------------

fn get_words_lossless(
    bs:            &mut Bits<'_>,
    ws:            &mut WordsState,
    flags:         u32,
    block_samples: u32,
) -> Option<Vec<i32>> {
    let is_mono  = (flags & MONO_DATA) != 0;
    // The C reference doubles nsamples for stereo and iterates one sample at a time.
    let nsamples = if is_mono { block_samples } else { block_samples.saturating_mul(2) };
    let mut buffer = vec![0i32; nsamples as usize];
    let mut csamples: u32 = 0;

    wp_trace!("[gwl] nsamples={} is_mono={} med0={} med1={} med1b={} holding_one={} zeros_acc={}",
        nsamples, is_mono,
        ws.c[0].median[0], ws.c[0].median[1],
        ws.c[1].median[0],
        ws.holding_one, ws.zeros_acc);

    while csamples < nsamples {
        // Select the entropy channel: even→L(0), odd→R(1) for stereo.
        let chan = if is_mono { 0usize } else { (csamples & 1) as usize };

        // ---- holding_zero fast path (from previous iteration's split) ----
        if ws.holding_zero != 0 {
            ws.holding_zero = 0;
            let med0 = ws.c[chan].median[0];
            let low  = read_code(bs, get_med(med0).saturating_sub(1));
            dec_med0(&mut ws.c[chan]);
            let samp = if bs.getbit() != 0 { !(low as i32) } else { low as i32 };
            if csamples < 10 { wp_trace!("[gwl] cs={} holding_zero path low={} val={}", csamples, low, samp); }
            buffer[csamples as usize] = samp;
            csamples += 1;
            continue;
        }

        // ---- zero-run check (both medians near zero and not mid-run) ----
        let c0_med = ws.c[0].median[0];
        let c1_med = ws.c[1].median[0];
        if c0_med < 2 && ws.holding_one == 0 && (is_mono || c1_med < 2) {
            if ws.zeros_acc != 0 {
                // Still inside a zero run
                ws.zeros_acc -= 1;
                if ws.zeros_acc != 0 {
                    if csamples < 10 { wp_trace!("[gwl] cs={} zero-run emit (zeros_acc={})", csamples, ws.zeros_acc); }
                    buffer[csamples as usize] = 0;
                    csamples += 1;
                    continue;
                }
                // zeros_acc just hit 0 — fall through to normal decode
            } else {
                // Read a zero-run count from the bitstream
                ws.zeros_acc = match read_ext_count(bs) {
                    Some(v) => v,
                    None => { wp_trace!("[gwl] cs={} EOS (cbits=33)", csamples); break; }
                };

                wp_trace!("[gwl] cs={} zeros path zeros_acc={}", csamples, ws.zeros_acc);

                if ws.zeros_acc != 0 {
                    // Reset both channels' medians then emit one zero sample
                    ws.c[0].median = [0; 3];
                    ws.c[1].median = [0; 3];
                    buffer[csamples as usize] = 0;
                    csamples += 1;
                    continue;
                }
                // zeros_acc == 0 → no run, fall through to decode a real sample
            }
        }

        // ---- count ones (with extended range for large values) ----
        let ones_raw = match count_ones_lim(bs) {
            Some(v) => v,
            None    => break,
        };

        // ---- holding_one / holding_zero state machine ----
        let low_hold        = ws.holding_one;
        ws.holding_one      = ones_raw & 1;
        ws.holding_zero     = (!(ones_raw) & 1) as i32;
        let ones_count      = (ones_raw >> 1) + low_hold;

        // ---- select median interval ----
        let c = &mut ws.c[chan];
        let (low, high): (u32, u32) = if ones_count == 0 {
            let h = get_med(c.median[0]).saturating_sub(1);
            dec_med0(c);
            (0, h)
        } else {
            let l = get_med(c.median[0]);
            inc_med0(c);
            if ones_count == 1 {
                let h = l.wrapping_add(get_med(c.median[1])).saturating_sub(1);
                dec_med1(c);
                (l, h)
            } else {
                let l2 = l.wrapping_add(get_med(c.median[1]));
                inc_med1(c);
                if ones_count == 2 {
                    let h = l2.wrapping_add(get_med(c.median[2])).saturating_sub(1);
                    dec_med2(c);
                    (l2, h)
                } else {
                    let add = (ones_count - 2).wrapping_mul(get_med(c.median[2]));
                    let l3  = l2.wrapping_add(add);
                    let h   = l3.wrapping_add(get_med(c.median[2])).saturating_sub(1);
                    inc_med2(c);
                    (l3, h)
                }
            }
        };

        let value = low.wrapping_add(read_code(bs, high.wrapping_sub(low)));
        let samp = if bs.getbit() != 0 { !(value as i32) } else { value as i32 };
        if csamples < 10 {
            wp_trace!("[gwl] cs={} ones_count={} low={} high={} value={} samp={}",
                csamples, ones_count, low, high, value, samp);
        }
        buffer[csamples as usize] = samp;
        csamples += 1;
    }

    wp_trace!("[gwl] done: decoded {} of {} samples", csamples, nsamples);
    Some(buffer)
}

// ---------------------------------------------------------------------------
// get_words_hybrid: entropy-decode a hybrid (lossy or hybrid-lossless) block.
// Structurally identical to get_words_lossless (same zero-run / holding_one
// fast paths — ported from get_word()/get_words_lossless() in read_words.c),
// but narrows the [low, high) median interval down to `error_limit` via the
// main bitstream instead of resolving it exactly with read_code(). Without a
// `.wvc` correction bitstream there is no second bitstream to refine `mid` into
// the exact lossless value, so the result is the lossy approximation, matching
// `wvunpack` run without its companion `.wvc`. With one, the exact value is read
// from it and returned as a per-word correction (see `unpack_hybrid_lossless`).
// ---------------------------------------------------------------------------

const SLS: u32 = 8;
const SLO: u32 = 1 << (SLS - 1);

/// The result of [`get_words_hybrid`].
struct HybridWords {
    /// The (possibly lossy) words, interleaved for stereo.
    samples: Vec<i32>,
    /// The per-word correction from the `.wvc` bitstream (same layout as `samples`); empty if
    /// no correction bitstream was given.
    corrections: Vec<i32>,
    /// The number of words that were decoded before the bitstream ended.
    decoded: u32,
}

/// Port of `get_word()` in read_words.c, run over a whole block. If `wvc` is given, the
/// correction bitstream is consumed too (when the `error_limit` of a word is non-zero) and
/// the resulting correction offsets are returned.
fn get_words_hybrid(
    bs:            &mut Bits<'_>,
    ws:            &mut WordsState,
    flags:         u32,
    block_samples: u32,
    mut wvc:       Option<&mut Bits<'_>>,
) -> Option<HybridWords> {
    let is_mono  = (flags & MONO_DATA) != 0;
    let nsamples = if is_mono { block_samples } else { block_samples.saturating_mul(2) };
    let mut buffer = vec![0i32; nsamples as usize];
    let mut corrections = if wvc.is_some() { vec![0i32; nsamples as usize] } else { Vec::new() };
    let mut csamples: u32 = 0;

    while csamples < nsamples {
        let chan = if is_mono { 0usize } else { (csamples & 1) as usize };

        // `holding_zero` here is the single-bit carry from the previous sample's ones-count
        // parity (distinct from the `zeros_acc` "run" below). Unlike `get_words_lossless()`,
        // `get_word()` has no early-resolve shortcut for it: it just forces `ones_count = 0`
        // and falls through to the ones_count==0 window below, going through the same
        // error_limit narrowing as every other value (crucial for hybrid: a value narrowed
        // via holding_zero is NOT necessarily exactly zero).
        let ones_count: u32;
        if ws.holding_zero != 0 {
            ws.holding_zero = 0;
            ones_count = 0;
        } else {
            let c0_med = ws.c[0].median[0];
            let c1_med = ws.c[1].median[0];
            if c0_med < 2 && ws.holding_one == 0 && (is_mono || c1_med < 2) {
                if ws.zeros_acc != 0 {
                    ws.zeros_acc -= 1;
                    if ws.zeros_acc != 0 {
                        if (flags & HYBRID_BITRATE) != 0 {
                            let c = &mut ws.c[chan];
                            c.slow_level = c.slow_level.wrapping_sub((c.slow_level.wrapping_add(SLO)) >> SLS);
                        }
                        buffer[csamples as usize] = 0;
                        csamples += 1;
                        continue;
                    }
                } else {
                    ws.zeros_acc = match read_ext_count(bs) {
                        Some(v) => v,
                        None => break,
                    };

                    if ws.zeros_acc != 0 {
                        if (flags & HYBRID_BITRATE) != 0 {
                            let c = &mut ws.c[chan];
                            c.slow_level = c.slow_level.wrapping_sub((c.slow_level.wrapping_add(SLO)) >> SLS);
                        }
                        ws.c[0].median = [0; 3];
                        ws.c[1].median = [0; 3];
                        buffer[csamples as usize] = 0;
                        csamples += 1;
                        continue;
                    }
                }
            }

            let ones_raw = match count_ones_lim(bs) {
                Some(v) => v,
                None    => break,
            };
            let low_hold        = ws.holding_one;
            ws.holding_one      = ones_raw & 1;
            ws.holding_zero     = (!(ones_raw) & 1) as i32;
            ones_count = (ones_raw >> 1) + low_hold;
        }

        // Ported from `if ((wps->wphdr.flags & HYBRID_FLAG) && !chan) update_error_limit(wps);`
        if chan == 0 {
            update_error_limit(ws, flags);
        }

        let c = &mut ws.c[chan];
        let (mut low, mut high): (u32, u32) = if ones_count == 0 {
            let h = get_med(c.median[0]).saturating_sub(1);
            dec_med0(c);
            (0, h)
        } else {
            let l = get_med(c.median[0]);
            inc_med0(c);
            if ones_count == 1 {
                let h = l.wrapping_add(get_med(c.median[1])).saturating_sub(1);
                dec_med1(c);
                (l, h)
            } else {
                let l2 = l.wrapping_add(get_med(c.median[1]));
                inc_med1(c);
                if ones_count == 2 {
                    let h = l2.wrapping_add(get_med(c.median[2])).saturating_sub(1);
                    dec_med2(c);
                    (l2, h)
                } else {
                    let add = (ones_count - 2).wrapping_mul(get_med(c.median[2]));
                    let l3  = l2.wrapping_add(add);
                    let h   = l3.wrapping_add(get_med(c.median[2])).saturating_sub(1);
                    inc_med2(c);
                    (l3, h)
                }
            }
        };

        low &= 0x7fff_ffff;
        high &= 0x7fff_ffff;
        if low > high { high = low; }

        let error_limit = ws.c[chan].error_limit;
        let (mut lo, mut hi) = (low, high);
        let mid = if error_limit == 0 {
            read_code(bs, high - low) + low
        } else {
            let mut m = (hi + lo + 1) >> 1;
            while hi - lo > error_limit {
                if bs.getbit() != 0 {
                    lo = m;
                    m = (hi + lo + 1) >> 1;
                } else {
                    hi = m - 1;
                    m = (hi + lo + 1) >> 1;
                }
            }
            m
        };

        let sign = bs.getbit();
        if csamples < 40 {
            wp_trace!("[getword] n={} chan={} error_limit={} low={} high={} mid={} sign={} out={}",
                csamples, chan, error_limit, low, high, mid, sign,
                if sign != 0 { !(mid as i32) } else { mid as i32 });
        }

        // The correction bitstream pins down the exact value inside the range that is left
        // once the main bitstream has been consumed.
        if error_limit != 0 {
            if let Some(wvc) = wvc.as_deref_mut() {
                let value = read_code(wvc, hi - lo) + lo;
                corrections[csamples as usize] =
                    if sign != 0 { mid.wrapping_sub(value) } else { value.wrapping_sub(mid) } as i32;
            }
        }

        if (flags & HYBRID_BITRATE) != 0 {
            let c = &mut ws.c[chan];
            c.slow_level = c.slow_level.wrapping_sub((c.slow_level.wrapping_add(SLO)) >> SLS);
            c.slow_level = c.slow_level.wrapping_add(wp_log2(mid) as u32);
        }

        buffer[csamples as usize] = if sign != 0 { !(mid as i32) } else { mid as i32 };
        csamples += 1;
    }

    Some(HybridWords { samples: buffer, corrections, decoded: csamples })
}

// ---------------------------------------------------------------------------
// Weight helpers (wavpack_local.h)
// ---------------------------------------------------------------------------

#[inline(always)]
fn apply_weight(weight: i32, sample: i32) -> i32 {
    ((weight as i64 * sample as i64 + 512) >> 10) as i32
}

#[inline(always)]
fn update_weight(weight: &mut i32, delta: i32, source: i32, result: i32) {
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        *weight = (delta ^ s) + (*weight - s);
    }
}

#[inline(always)]
fn update_weight_clip(weight: &mut i32, delta: i32, source: i32, result: i32) {
    // Match the C macro in wavpack_local.h exactly:
    //   if (source && result) {
    //     const int32_t s = (source ^ result) >> 31;
    //     if ((weight = (weight ^ s) + (delta - s)) > 1024) weight = 1024;
    //     weight = (weight ^ s) - s;
    //   }
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        let mut w = (*weight ^ s).wrapping_add(delta.wrapping_sub(s));
        if w > 1024 { w = 1024; }
        *weight = (w ^ s).wrapping_sub(s);
    }
}

// ---------------------------------------------------------------------------
// Decorrelation passes (unpack.c decorr_stereo_pass / decorr_mono_pass)
// ---------------------------------------------------------------------------

/// Apply all the decorrelation passes (in order) to a stereo block of interleaved samples, then
/// undo the joint stereo (if `joint`).
///
/// The passes are fused into one loop over the frames. Every pass is a serial chain (the weight
/// update and the history feed back into the next frame), but pass `j + 1` of frame `i` only
/// depends on pass `j` of frame `i`, so the chains of the passes overlap in the out-of-order
/// window instead of running one after the other over the whole buffer. The result is identical
/// to running each pass over the whole buffer.
pub fn decorr_stereo_passes(passes: &mut [DecorrPass], buf: &mut [i32], joint: bool) {
    for (i, frame) in buf.chunks_exact_mut(2).enumerate() {
        let m = i & (MAX_TERM - 1);
        let (mut a, mut b) = (frame[0], frame[1]);

        for p in passes.iter_mut() {
            match p.term {
                t if t > 0 && t <= MAX_TERM as i32 => {
                    let sam_a = p.samples_a[m];
                    let sam_b = p.samples_b[m];
                    let k = (m + t as usize) & (MAX_TERM - 1);

                    let na = a.wrapping_add(apply_weight(p.weight_a, sam_a));
                    let nb = b.wrapping_add(apply_weight(p.weight_b, sam_b));
                    update_weight(&mut p.weight_a, p.delta, sam_a, a);
                    update_weight(&mut p.weight_b, p.delta, sam_b, b);
                    p.samples_a[k] = na;
                    p.samples_b[k] = nb;
                    a = na;
                    b = nb;
                }
                17 => {
                    let sa = (2i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1]);
                    let sb = (2i32).wrapping_mul(p.samples_b[0]).wrapping_sub(p.samples_b[1]);
                    p.samples_a[1] = p.samples_a[0];
                    p.samples_b[1] = p.samples_b[0];

                    let na = a.wrapping_add(apply_weight(p.weight_a, sa));
                    let nb = b.wrapping_add(apply_weight(p.weight_b, sb));
                    update_weight(&mut p.weight_a, p.delta, sa, a);
                    update_weight(&mut p.weight_b, p.delta, sb, b);
                    p.samples_a[0] = na;
                    p.samples_b[0] = nb;
                    a = na;
                    b = nb;
                }
                18 => {
                    let sa = ((3i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1])) >> 1;
                    let sb = ((3i32).wrapping_mul(p.samples_b[0]).wrapping_sub(p.samples_b[1])) >> 1;
                    p.samples_a[1] = p.samples_a[0];
                    p.samples_b[1] = p.samples_b[0];

                    let na = a.wrapping_add(apply_weight(p.weight_a, sa));
                    let nb = b.wrapping_add(apply_weight(p.weight_b, sb));
                    update_weight(&mut p.weight_a, p.delta, sa, a);
                    update_weight(&mut p.weight_b, p.delta, sb, b);
                    p.samples_a[0] = na;
                    p.samples_b[0] = nb;
                    a = na;
                    b = nb;
                }
                -1 => {
                    let sam = a.wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                    update_weight_clip(&mut p.weight_a, p.delta, p.samples_a[0], a);
                    let nb = b.wrapping_add(apply_weight(p.weight_b, sam));
                    update_weight_clip(&mut p.weight_b, p.delta, sam, b);
                    p.samples_a[0] = nb;
                    a = sam;
                    b = nb;
                }
                -2 => {
                    let sam = b.wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));
                    update_weight_clip(&mut p.weight_b, p.delta, p.samples_b[0], b);
                    let na = a.wrapping_add(apply_weight(p.weight_a, sam));
                    update_weight_clip(&mut p.weight_a, p.delta, sam, a);
                    p.samples_b[0] = na;
                    a = na;
                    b = sam;
                }
                -3 => {
                    let sam_a = a.wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                    update_weight_clip(&mut p.weight_a, p.delta, p.samples_a[0], a);
                    let nb = b.wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));
                    update_weight_clip(&mut p.weight_b, p.delta, p.samples_b[0], b);
                    p.samples_a[0] = nb;
                    p.samples_b[0] = sam_a;
                    a = sam_a;
                    b = nb;
                }
                _ => {}
            }
        }

        if joint {
            // Undo joint stereo: `a += (b -= (a >> 1))`.
            b = b.wrapping_sub(a >> 1);
            a = a.wrapping_add(b);
        }

        frame[0] = a;
        frame[1] = b;
    }
}

/// Apply all the decorrelation passes (in order) to a mono block; see [`decorr_stereo_passes`].
pub fn decorr_mono_passes(passes: &mut [DecorrPass], buf: &mut [i32]) {
    for (i, x) in buf.iter_mut().enumerate() {
        let m = i & (MAX_TERM - 1);
        let mut a = *x;

        for p in passes.iter_mut() {
            match p.term {
                t if t > 0 && t <= MAX_TERM as i32 => {
                    let sam = p.samples_a[m];
                    let k = (m + t as usize) & (MAX_TERM - 1);
                    let na = a.wrapping_add(apply_weight(p.weight_a, sam));
                    update_weight(&mut p.weight_a, p.delta, sam, a);
                    p.samples_a[k] = na;
                    a = na;
                }
                17 => {
                    let sa = (2i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1]);
                    p.samples_a[1] = p.samples_a[0];
                    let na = a.wrapping_add(apply_weight(p.weight_a, sa));
                    update_weight(&mut p.weight_a, p.delta, sa, a);
                    p.samples_a[0] = na;
                    a = na;
                }
                18 => {
                    let sa = ((3i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1])) >> 1;
                    p.samples_a[1] = p.samples_a[0];
                    let na = a.wrapping_add(apply_weight(p.weight_a, sa));
                    update_weight(&mut p.weight_a, p.delta, sa, a);
                    p.samples_a[0] = na;
                    a = na;
                }
                _ => {}
            }
        }

        *x = a;
    }
}

/// The "extension" (`ID_WVX_BITSTREAM` / `ID_WVX_NEW_BITSTREAM`) bitstream of a block: the
/// bits needed to losslessly restore 32-bit float data or integer data wider than 24 bits.
/// It lives in the `.wv` block for ordinary lossless files and in the `.wvc` block for
/// hybrid-lossless ones.
#[derive(Clone, Copy)]
pub struct WvxInput<'a> {
    /// The CRC of the restored extended data, stored in the first 4 bytes of the sub-block.
    pub crc:    u32,
    /// The bitstream, following the CRC.
    pub bits:   &'a [u8],
    /// `ID_WVX_NEW_BITSTREAM`: the bitstream starts with one (integer data) or two (float
    /// data) 5-bit fields.
    pub is_new: bool,
}

impl<'a> WvxInput<'a> {
    /// Port of `init_wvx_bitstream()` in open_utils.c (validity check and CRC extraction).
    pub fn parse(raw: &'a [u8], is_new: bool) -> Option<Self> {
        if raw.len() <= 4 {
            return None;
        }
        let crc = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        Some(WvxInput { crc, bits: &raw[4..], is_new })
    }
}

/// The correction (`ID_WVC_BITSTREAM`) data of a block that comes from the `.wvc` file.
pub struct WvcInput<'a> {
    /// The correction bitstream.
    pub bits:    &'a [u8],
    /// The CRC of the *lossless* output, from the header of the correction block.
    pub crc:     u32,
    /// `ID_SHAPING_WEIGHTS` of the correction block.
    pub shaping: &'a [u8],
    /// `ID_WVX_BITSTREAM` of the correction block, if any.
    pub wvx:     Option<WvxInput<'a>>,
}

/// Open a `WvxInput` as a bit reader and consume its new-format prefix, returning the reader
/// and the `(float_min_shifted_zeros, float_max_shifted_ones, int32_max_width)` fields.
fn open_wvx<'a>(wvx: &WvxInput<'a>, flags: u32) -> (Bits<'a>, i32, i32, u32) {
    let mut bits = Bits::new(wvx.bits);
    let (mut min_zeros, mut max_ones, mut max_width) = (0, 0, 0);
    if wvx.is_new {
        if (flags & FLOAT_DATA) != 0 {
            min_zeros = (bits.getbits(5) & 0x1f) as i32;
            max_ones = (bits.getbits(5) & 0x1f) as i32;
        }
        else {
            max_width = bits.getbits(5) & 0x1f;
        }
    }
    (bits, min_zeros, max_ones, max_width)
}

/// Final integer fixup: INT32_DATA extra-bit restoration, hybrid-lossy clipping and
/// the residual precision shift. Not used for `FLOAT_DATA` blocks (see `floats::float_values`).
/// Port of the (non-float) tail of `fixup_samples()` in unpack.c.
///
/// `lossy` is `HYBRID_FLAG` without a correction block. `wvx`, if given, supplies the bits
/// of a lossless 32-bit integer stream; the CRC of the restored data is returned in that case.
pub fn fixup_samples(
    buf: &mut [i32],
    flags: u32,
    i32info: &Int32Info,
    lossy: bool,
    wvx: Option<&WvxInput<'_>>,
) -> Option<u32> {
    let mut shift = ((flags & SHIFT_MASK) >> SHIFT_LSB) as i32;
    let mut crc_x = None;

    if (flags & INT32_DATA) != 0 {
        let sent_bits = (i32info.sent_bits & 0x1f) as u32;
        let mut zeros = (i32info.zeros & 0x1f) as u32;
        let mut ones = (i32info.ones & 0x1f) as u32;
        let mut dups = (i32info.dups & 0x1f) as u32;
        let mask = (1u32 << sent_bits).wrapping_sub(1);

        // Restore the "zeros"/"ones"/"dups" bits of one sample.
        let restore = |v: i32, zeros: u32, ones: u32, dups: u32| -> i32 {
            if zeros != 0 {
                ((v as u32) << zeros) as i32
            }
            else if ones != 0 {
                (((v.wrapping_add(1)) as u32) << ones).wrapping_sub(1) as i32
            }
            else if dups != 0 {
                let low = v & 1;
                (((v.wrapping_add(low)) as u32) << dups).wrapping_sub(low as u32) as i32
            }
            else {
                v
            }
        };

        if let Some(wvx) = wvx {
            let (mut bits, _, _, max_width) = open_wvx(wvx, flags);
            let mut crc: u32 = 0xffff_ffff;

            for s in buf.iter_mut() {
                if sent_bits != 0 {
                    if max_width != 0 {
                        let pvalue = if *s < 0 { !*s } else { *s } as u32;
                        let width = count_bits(pvalue) + sent_bits;
                        let mut bits_to_read = sent_bits as i32;

                        let read = width <= max_width || {
                            bits_to_read -= (width - max_width) as i32;
                            bits_to_read > 0
                        };

                        if read {
                            let data = bits.getbits(bits_to_read as u32) & ((1u32 << bits_to_read) - 1);
                            *s = ((((*s as u32) << bits_to_read) | data)
                                << (sent_bits as i32 - bits_to_read)) as i32;
                        }
                        else {
                            *s = ((*s as u32) << sent_bits) as i32;
                        }
                    }
                    else {
                        let data = bits.getbits(sent_bits);
                        *s = (((*s as u32) << sent_bits) | (data & mask)) as i32;
                    }
                }

                *s = restore(*s, zeros, ones, dups);

                let v = *s as u32;
                crc = crc
                    .wrapping_mul(9)
                    .wrapping_add((v & 0xffff).wrapping_mul(3))
                    .wrapping_add((v >> 16) & 0xffff);
            }

            crc_x = Some(crc);
        }
        else if sent_bits == 0 && (zeros + ones + dups) != 0 {
            while lossy && (flags & 3) == 3 && shift < 8 {
                if zeros != 0 {
                    zeros -= 1;
                }
                else if ones != 0 {
                    ones -= 1;
                }
                else if dups != 0 {
                    dups -= 1;
                }
                else {
                    break;
                }
                shift += 1;
            }

            for s in buf.iter_mut() {
                *s = restore(*s, zeros, ones, dups);
            }
        }
        else {
            shift += (zeros + sent_bits + ones + dups) as i32;
        }
    }

    let shift = (shift & 0x1f) as u32;

    if lossy {
        // Clip to the original (pre-shift) sample range, then restore precision. Port of
        // the `lossy_flag` branch of `fixup_samples()` in unpack.c.
        let (min_value, max_value): (i32, i32) = match flags & 3 {
            0 => (-128i32 >> shift, 127i32 >> shift),
            1 => (-32768i32 >> shift, 32767i32 >> shift),
            2 => (-8_388_608i32 >> shift, 8_388_607i32 >> shift),
            _ => (i32::MIN >> shift, i32::MAX >> shift),
        };
        let min_shifted = ((min_value as u32) << shift) as i32;
        let max_shifted = ((max_value as u32) << shift) as i32;

        for s in buf.iter_mut() {
            *s = if *s < min_value {
                min_shifted
            } else if *s > max_value {
                max_shifted
            } else {
                ((*s as u32) << shift) as i32
            };
        }
    } else if shift != 0 {
        for s in buf.iter_mut() {
            *s = ((*s as u32) << shift) as i32;
        }
    }

    crc_x
}

// ---------------------------------------------------------------------------
// Hybrid-lossless decoding (main bitstream + .wvc correction bitstream)
// ---------------------------------------------------------------------------

/// Noise-shaping state of a hybrid block (`wps->dc` in wavpack_local.h), restored from
/// `ID_SHAPING_WEIGHTS`.
#[derive(Default, Clone, Copy)]
pub struct ShapingState {
    pub acc:   [i32; 2],
    pub delta: [i32; 2],
    pub error: [i32; 2],
}

/// Port of `read_shaping_info()` in decorr_utils.c. Returns `false` for a malformed sub-block.
pub fn parse_shaping_info(data: &[u8], flags: u32, dc: &mut ShapingState) -> bool {
    let is_mono = (flags & MONO_DATA) != 0;

    if data.len() == 2 {
        dc.acc[0] = ((restore_weight(data[0] as i8) as u32) << 16) as i32;
        dc.acc[1] = ((restore_weight(data[1] as i8) as u32) << 16) as i32;
        return true;
    }

    if data.len() >= if is_mono { 4 } else { 8 } {
        let r = |off: usize| wp_exp2s(i16::from_le_bytes([data[off], data[off + 1]]) as i32);

        dc.error[0] = r(0);
        dc.acc[0] = r(2);
        let mut off = 4;

        if !is_mono {
            dc.error[1] = r(4);
            dc.acc[1] = r(6);
            off = 8;
        }

        if data.len() == if is_mono { 6 } else { 12 } {
            dc.delta[0] = r(off);
            if !is_mono {
                dc.delta[1] = r(off + 2);
            }
        }

        return true;
    }

    false
}

/// One step of noise shaping for channel `ch` (the shared tail of the hybrid-lossless loops in
/// `unpack_samples()`): updates `dc.error[ch]` and returns the shaping offset `temp`.
#[inline(always)]
fn shaping_step(dc: &mut ShapingState, ch: usize, flags: u32, correction: i32) -> i32 {
    dc.acc[ch] = dc.acc[ch].wrapping_add(dc.delta[ch]);
    let shaping_weight = dc.acc[ch] >> 16;
    let mut temp = apply_weight(shaping_weight, dc.error[ch]).wrapping_neg();

    if (flags & NEW_SHAPING) != 0 && shaping_weight < 0 && temp != 0 {
        if temp == dc.error[ch] {
            temp = if temp < 0 { temp + 1 } else { temp - 1 };
        }
        dc.error[ch] = temp.wrapping_sub(correction);
    }
    else {
        dc.error[ch] = correction.wrapping_neg();
    }

    temp
}

/// Reconstruct the lossless samples of a hybrid block from its main bitstream, correction
/// bitstream and the decorrelation passes. Port of the two "hybrid lossless" branches of
/// `unpack_samples()` in unpack.c (which interleave entropy decoding with the passes; the
/// entropy decoder is independent of the passes, so decoding all words up front is
/// equivalent).
///
/// Returns the samples (before the final shift) and the CRC computed over them, or `None` if
/// a bitstream ended early or a sample is implausibly large (the reference "mutes" the block
/// in those cases).
fn unpack_hybrid_lossless(
    flags: u32,
    block_samples: u32,
    passes: &mut [DecorrPass],
    ws: &mut WordsState,
    audio: &[u8],
    wvc: &WvcInput<'_>,
) -> Option<(Vec<i32>, u32)> {
    let is_mono = (flags & MONO_DATA) != 0;
    let n = block_samples as usize;

    let mut dc = ShapingState::default();
    if !wvc.shaping.is_empty() {
        parse_shaping_info(wvc.shaping, flags, &mut dc);
    }

    let mut bs = Bits::new(audio);
    let mut wvc_bs = Bits::new(wvc.bits);
    let words = get_words_hybrid(&mut bs, ws, flags, block_samples, Some(&mut wvc_bs))?;
    let nsamples = if is_mono { n } else { n * 2 };
    if words.decoded as usize != nsamples {
        return None;
    }
    let (words, corr) = (words.samples, words.corrections);

    let mag = (flags & MAG_MASK) >> MAG_LSB;
    let mute_limit: i64 = (1i64 << mag) + 2;
    let mut crc: u32 = 0xffff_ffff;
    let mut out: Vec<i32> = Vec::with_capacity(nsamples);
    let mut m = 0usize;

    if is_mono {
        for i in 0..n {
            let mut read_word = words[i];
            let correction = corr[i];

            for p in passes.iter_mut() {
                let sam;
                let k;
                if p.term > MAX_TERM as i32 {
                    sam = if (p.term & 1) != 0 {
                        p.samples_a[0].wrapping_mul(2).wrapping_sub(p.samples_a[1])
                    }
                    else {
                        p.samples_a[0].wrapping_mul(3).wrapping_sub(p.samples_a[1]) >> 1
                    };
                    p.samples_a[1] = p.samples_a[0];
                    k = 0;
                }
                else {
                    sam = p.samples_a[m];
                    k = (m + p.term as usize) & (MAX_TERM - 1);
                }

                let temp = apply_weight(p.weight_a, sam).wrapping_add(read_word);
                update_weight(&mut p.weight_a, p.delta, sam, read_word);
                read_word = temp;
                p.samples_a[k] = temp;
            }

            m = (m + 1) & (MAX_TERM - 1);

            if (flags & HYBRID_SHAPE) != 0 {
                let temp = shaping_step(&mut dc, 0, flags, correction);
                read_word = read_word.wrapping_add(correction).wrapping_sub(temp);
            }
            else {
                read_word = read_word.wrapping_add(correction);
            }

            crc = crc.wrapping_add(crc << 1).wrapping_add(read_word as u32);

            if (read_word as i64).abs() > mute_limit {
                return None;
            }
            out.push(read_word);
        }
    }
    else {
        for i in 0..n {
            let mut left = words[i * 2];
            let mut right = words[i * 2 + 1];
            let correction = [corr[i * 2], corr[i * 2 + 1]];
            let (mut left_c, mut right_c) = (0i32, 0i32);

            if (flags & CROSS_DECORR) != 0 {
                left_c = left.wrapping_add(correction[0]);
                right_c = right.wrapping_add(correction[1]);

                for p in passes.iter() {
                    if p.term > 0 {
                        let (sam_a, sam_b);
                        if p.term > MAX_TERM as i32 {
                            if (p.term & 1) != 0 {
                                sam_a = p.samples_a[0].wrapping_mul(2).wrapping_sub(p.samples_a[1]);
                                sam_b = p.samples_b[0].wrapping_mul(2).wrapping_sub(p.samples_b[1]);
                            }
                            else {
                                sam_a = p.samples_a[0].wrapping_mul(3).wrapping_sub(p.samples_a[1]) >> 1;
                                sam_b = p.samples_b[0].wrapping_mul(3).wrapping_sub(p.samples_b[1]) >> 1;
                            }
                        }
                        else {
                            sam_a = p.samples_a[m];
                            sam_b = p.samples_b[m];
                        }

                        left_c = left_c.wrapping_add(apply_weight(p.weight_a, sam_a));
                        right_c = right_c.wrapping_add(apply_weight(p.weight_b, sam_b));
                    }
                    else if p.term == -1 {
                        left_c = left_c.wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                        right_c = right_c.wrapping_add(apply_weight(p.weight_b, left_c));
                    }
                    else {
                        right_c = right_c.wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));

                        if p.term == -3 {
                            left_c = left_c.wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                        }
                        else {
                            left_c = left_c.wrapping_add(apply_weight(p.weight_a, right_c));
                        }
                    }
                }

                if (flags & JOINT_STEREO) != 0 {
                    right_c = right_c.wrapping_sub(left_c >> 1);
                    left_c = left_c.wrapping_add(right_c);
                }
            }

            for p in passes.iter_mut() {
                if p.term > 0 {
                    let (sam_a, sam_b, k);
                    if p.term > MAX_TERM as i32 {
                        if (p.term & 1) != 0 {
                            sam_a = p.samples_a[0].wrapping_mul(2).wrapping_sub(p.samples_a[1]);
                            sam_b = p.samples_b[0].wrapping_mul(2).wrapping_sub(p.samples_b[1]);
                        }
                        else {
                            sam_a = p.samples_a[0].wrapping_mul(3).wrapping_sub(p.samples_a[1]) >> 1;
                            sam_b = p.samples_b[0].wrapping_mul(3).wrapping_sub(p.samples_b[1]) >> 1;
                        }
                        p.samples_a[1] = p.samples_a[0];
                        p.samples_b[1] = p.samples_b[0];
                        k = 0;
                    }
                    else {
                        sam_a = p.samples_a[m];
                        sam_b = p.samples_b[m];
                        k = (m + p.term as usize) & (MAX_TERM - 1);
                    }

                    let left2 = apply_weight(p.weight_a, sam_a).wrapping_add(left);
                    let right2 = apply_weight(p.weight_b, sam_b).wrapping_add(right);

                    update_weight(&mut p.weight_a, p.delta, sam_a, left);
                    update_weight(&mut p.weight_b, p.delta, sam_b, right);

                    left = left2;
                    right = right2;
                    p.samples_a[k] = left;
                    p.samples_b[k] = right;
                }
                else if p.term == -1 {
                    let left2 = left.wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                    update_weight_clip(&mut p.weight_a, p.delta, p.samples_a[0], left);
                    left = left2;
                    let right2 = right.wrapping_add(apply_weight(p.weight_b, left2));
                    update_weight_clip(&mut p.weight_b, p.delta, left2, right);
                    right = right2;
                    p.samples_a[0] = right;
                }
                else {
                    let mut right2 = right.wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));
                    update_weight_clip(&mut p.weight_b, p.delta, p.samples_b[0], right);
                    right = right2;

                    if p.term == -3 {
                        right2 = p.samples_a[0];
                        p.samples_a[0] = right;
                    }

                    let left2 = left.wrapping_add(apply_weight(p.weight_a, right2));
                    update_weight_clip(&mut p.weight_a, p.delta, right2, left);
                    left = left2;
                    p.samples_b[0] = left;
                }
            }

            m = (m + 1) & (MAX_TERM - 1);

            if (flags & CROSS_DECORR) == 0 {
                left_c = left.wrapping_add(correction[0]);
                right_c = right.wrapping_add(correction[1]);

                if (flags & JOINT_STEREO) != 0 {
                    right_c = right_c.wrapping_sub(left_c >> 1);
                    left_c = left_c.wrapping_add(right_c);
                }
            }

            if (flags & JOINT_STEREO) != 0 {
                right = right.wrapping_sub(left >> 1);
                left = left.wrapping_add(right);
            }

            if (flags & HYBRID_SHAPE) != 0 {
                let c0 = left_c.wrapping_sub(left);
                let temp = shaping_step(&mut dc, 0, flags, c0);
                left = left_c.wrapping_sub(temp);

                let c1 = right_c.wrapping_sub(right);
                let temp = shaping_step(&mut dc, 1, flags, c1);
                right = right_c.wrapping_sub(temp);
            }
            else {
                left = left_c;
                right = right_c;
            }

            if (left as i64).abs() > mute_limit || (right as i64).abs() > mute_limit {
                return None;
            }

            crc = crc
                .wrapping_add(crc << 3)
                .wrapping_add((left as u32) << 1)
                .wrapping_add(left as u32)
                .wrapping_add(right as u32);
            out.push(left);
            out.push(right);
        }
    }

    Some((out, crc))
}

/// Hybrid-lossless decode of one block: samples plus the check of both CRCs. `None` means the
/// correction data does not reproduce the block (corrupt, truncated or mismatched `.wvc`).
#[allow(clippy::too_many_arguments)]
fn unpack_with_correction(
    flags: u32,
    block_samples: u32,
    passes: &mut [DecorrPass],
    ws: &mut WordsState,
    i32info: &Int32Info,
    float_info: Option<&super::floats::FloatInfo>,
    main_wvx: Option<WvxInput<'_>>,
    audio: &[u8],
    wvc: &WvcInput<'_>,
) -> Option<Vec<i32>> {
    let (mut buf, crc) = unpack_hybrid_lossless(flags, block_samples, passes, ws, audio, wvc)?;

    if crc != wvc.crc {
        wp_trace!("[wvc] CRC mismatch: computed {:08x}, expected {:08x}", crc, wvc.crc);
        return None;
    }

    // The extension bits come from the correction block, or from the main block for ordinary
    // lossless data.
    let wvx = wvc.wvx.or(main_wvx);

    let crc_x = if (flags & FLOAT_DATA) != 0 {
        let info = float_info.copied().unwrap_or_default();
        match &wvx {
            Some(wvx) => {
                let (mut bits, min_zeros, max_ones, _) = open_wvx(wvx, flags);
                let info = super::floats::FloatInfo {
                    min_shifted_zeros: min_zeros,
                    max_shifted_ones: max_ones,
                    ..info
                };
                super::floats::float_values(&mut buf, &info, Some(&mut bits))
            }
            None => {
                super::floats::float_values(&mut buf, &info, None);
                None
            }
        }
    }
    else {
        fixup_samples(&mut buf, flags, i32info, false, wvx.as_ref())
    };

    if let (Some(crc_x), Some(wvx)) = (crc_x, &wvx) {
        if crc_x != wvx.crc {
            wp_trace!("[wvc] WVX CRC mismatch: computed {:08x}, expected {:08x}", crc_x, wvx.crc);
            return None;
        }
    }

    Some(buf)
}

// ---------------------------------------------------------------------------
// Main entry point: decode one v4/v5 block
// ---------------------------------------------------------------------------

/// Decode one v4/v5 block.
///
/// If `wvc` (the matching block of the `.wvc` correction file) is given and the block is a
/// hybrid block, the lossless samples are reconstructed and verified against the CRC of the
/// correction block. Should that fail, the block is decoded from the main bitstream alone
/// (i.e. the lossy approximation) rather than being muted.
#[allow(clippy::too_many_arguments)]
pub fn unpack_samples_v4v5(
    flags:         u32,
    block_samples: u32,
    passes:        &mut [DecorrPass],
    ws:            &mut WordsState,
    i32info:       &Int32Info,
    float_info:    Option<&super::floats::FloatInfo>,
    wvx:           Option<WvxInput<'_>>,
    wvc:           Option<&WvcInput<'_>>,
    audio:         &[u8],
) -> Option<Vec<i32>> {
    // WavPack's own encoder caps block size at 131072 samples (`--blocksize`); reject
    // anything wildly larger up front rather than risk an unbounded allocation or an
    // arithmetic overflow further down from a malformed/mutated `block_samples` field
    // (this decoder must never panic on untrusted network-stream input).
    if block_samples > MAX_BLOCK_SAMPLES {
        return None;
    }

    if let Some(wvc) = wvc.filter(|_| (flags & HYBRID_FLAG) != 0) {
        let saved_passes = passes.to_vec();
        let saved_ws = ws.clone();

        if let Some(buf) = unpack_with_correction(
            flags, block_samples, passes, ws, i32info, float_info, wvx, audio, wvc,
        ) {
            return Some(buf);
        }

        log::warn!("wavpack: correction block does not match, decoding the lossy block only");
        passes.clone_from_slice(&saved_passes);
        *ws = saved_ws;
    }

    let is_mono = (flags & MONO_DATA) != 0;
    let mut bs  = Bits::new(audio);

    let mut buf = if (flags & HYBRID_FLAG) != 0 {
        get_words_hybrid(&mut bs, ws, flags, block_samples, None)?.samples
    } else {
        get_words_lossless(&mut bs, ws, flags, block_samples)?
    };

    // Apply the decorrelation passes in forward order, then undo the joint stereo.
    if is_mono {
        decorr_mono_passes(passes, &mut buf);
    } else {
        decorr_stereo_passes(passes, &mut buf, (flags & JOINT_STEREO) != 0);
    }

    if (flags & FLOAT_DATA) != 0 {
        wp_trace!("[float] wvx_present={} float_info_present={}", wvx.is_some(), float_info.is_some());
        let info = float_info.copied().unwrap_or_default();
        match &wvx {
            Some(wvx) => {
                let (mut bits, min_zeros, max_ones, _) = open_wvx(wvx, flags);
                let info = super::floats::FloatInfo {
                    min_shifted_zeros: min_zeros,
                    max_shifted_ones: max_ones,
                    ..info
                };
                super::floats::float_values(&mut buf, &info, Some(&mut bits));
            }
            None => {
                super::floats::float_values(&mut buf, &info, None);
            }
        }
    } else {
        fixup_samples(&mut buf, flags, i32info, (flags & HYBRID_FLAG) != 0, wvx.as_ref());
    }

    Some(buf)
}
