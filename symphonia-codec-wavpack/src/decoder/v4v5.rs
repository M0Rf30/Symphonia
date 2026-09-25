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
#[allow(dead_code)] // documents the bit; only meaningful when a .wvc correction file is
                     // available (unsupported by this fork, see README), so it never
                     // needs to be checked on the lossy-only decode path.
pub const HYBRID_SHAPE:   u32 = 0x0000_0040;
pub const FLOAT_DATA:     u32 = 0x0000_0080;
pub const INT32_DATA:     u32 = 0x0000_0100;
pub const HYBRID_BITRATE: u32 = 0x0000_0200;
pub const HYBRID_BALANCE: u32 = 0x0000_0400;
pub const SHIFT_LSB:      u32 = 13;
pub const SHIFT_MASK:     u32 = 0x1f << 13;
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
//   + hybrid_profile_len(4) + float_info_len(4) + int32_len(4) + wvx_len(4) = 44 bytes
pub const STREAM_HDR: usize = 44;

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

    #[inline(always)]
    pub(super) fn getbit(&mut self) -> u32 {
        if self.bc == 0 {
            let byte = self.next_byte() as u64;
            self.sr = byte;
            self.bc = 7;
            let bit = (self.sr & 1) as u32;
            self.sr >>= 1;
            bit
        } else {
            self.bc -= 1;
            let bit = (self.sr & 1) as u32;
            self.sr >>= 1;
            bit
        }
    }

    #[inline(always)]
    pub(super) fn getbits(&mut self, nbits: u32) -> u32 {
        if nbits == 0 { return 0; }
        while nbits > self.bc {
            let byte = self.next_byte() as u64;
            self.sr |= byte << self.bc;
            self.bc += 8;
        }
        let val = (self.sr & ((1u64 << nbits) - 1)) as u32;
        self.sr >>= nbits;
        self.bc -= nbits;
        val
    }

    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if self.ptr < self.data.len() {
            let b = self.data[self.ptr];
            self.ptr += 1;
            b
        } else {
            0x00
        }
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

fn count_ones_lim(bs: &mut Bits<'_>) -> Option<u32> {
    let mut ones_count: u32 = 0;
    while ones_count <= LIMIT_ONES && bs.getbit() == 1 {
        ones_count += 1;
    }

    if ones_count >= LIMIT_ONES {
        if ones_count > LIMIT_ONES { return None; } // 17+ consecutive ones = EOS

        // Extended range coding for large values
        let mut cbits: u32 = 0;
        while cbits < 33 && bs.getbit() == 1 { cbits += 1; }
        if cbits == 33 { return None; }

        if cbits < 2 {
            ones_count = cbits;
        } else {
            let mut mask = 1u32;
            ones_count = 0;
            let mut remaining = cbits - 1;
            while remaining > 0 {
                remaining -= 1;
                if bs.getbit() == 1 { ones_count |= mask; }
                mask <<= 1;
            }
            ones_count |= mask;
        }

        ones_count += LIMIT_ONES;
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
                let mut cbits: u32 = 0;
                while cbits < 33 && bs.getbit() == 1 { cbits += 1; }
                if cbits == 33 { wp_trace!("[gwl] cs={} EOS (cbits=33)", csamples); break; }

                if cbits < 2 {
                    ws.zeros_acc = cbits;
                } else {
                    let mut mask = 1u32;
                    ws.zeros_acc = 0;
                    let mut rem = cbits - 1;
                    while rem > 0 {
                        rem -= 1;
                        if bs.getbit() == 1 { ws.zeros_acc |= mask; }
                        mask <<= 1;
                    }
                    ws.zeros_acc |= mask;
                }

                wp_trace!("[gwl] cs={} zeros path cbits={} zeros_acc={}", csamples, cbits, ws.zeros_acc);

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
// `.wvc` correction file (unsupported by this fork — see README) there is no
// second bitstream to refine `mid` into the exact lossless value, so this
// always produces the lossy approximation, matching `wvunpack` run without
// its companion `.wvc`.
// ---------------------------------------------------------------------------

const SLS: u32 = 8;
const SLO: u32 = 1 << (SLS - 1);

fn get_words_hybrid(
    bs:            &mut Bits<'_>,
    ws:            &mut WordsState,
    flags:         u32,
    block_samples: u32,
) -> Option<Vec<i32>> {
    let is_mono  = (flags & MONO_DATA) != 0;
    let nsamples = if is_mono { block_samples } else { block_samples.saturating_mul(2) };
    let mut buffer = vec![0i32; nsamples as usize];
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
                    let mut cbits: u32 = 0;
                    while cbits < 33 && bs.getbit() == 1 { cbits += 1; }
                    if cbits == 33 { break; }

                    if cbits < 2 {
                        ws.zeros_acc = cbits;
                    } else {
                        let mut mask = 1u32;
                        ws.zeros_acc = 0;
                        let mut rem = cbits - 1;
                        while rem > 0 {
                            rem -= 1;
                            if bs.getbit() == 1 { ws.zeros_acc |= mask; }
                            mask <<= 1;
                        }
                        ws.zeros_acc |= mask;
                    }

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
        let mid = if error_limit == 0 {
            read_code(bs, high - low) + low
        } else {
            let mut lo = low;
            let mut hi = high;
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

        if (flags & HYBRID_BITRATE) != 0 {
            let c = &mut ws.c[chan];
            c.slow_level = c.slow_level.wrapping_sub((c.slow_level.wrapping_add(SLO)) >> SLS);
            c.slow_level = c.slow_level.wrapping_add(wp_log2(mid) as u32);
        }

        buffer[csamples as usize] = if sign != 0 { !(mid as i32) } else { mid as i32 };
        csamples += 1;
    }

    Some(buffer)
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

pub fn decorr_stereo_pass(p: &mut DecorrPass, buf: &mut [i32]) {
    let n = buf.len() / 2;
    let mut m = 0usize;

    match p.term {
        t if t > 0 && t <= MAX_TERM as i32 => {
            for i in 0..n {
                let sam_a = p.samples_a[m];
                let sam_b = p.samples_b[m];
                let k     = (m + t as usize) & (MAX_TERM - 1);

                let na = buf[i * 2    ].wrapping_add(apply_weight(p.weight_a, sam_a));
                let nb = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, sam_b));
                update_weight(&mut p.weight_a, p.delta, sam_a, buf[i * 2    ]);
                update_weight(&mut p.weight_b, p.delta, sam_b, buf[i * 2 + 1]);
                p.samples_a[k] = na; buf[i * 2    ] = na;
                p.samples_b[k] = nb; buf[i * 2 + 1] = nb;
                m = (m + 1) & (MAX_TERM - 1);
            }
        }
        17 => {
            for i in 0..n {
                let sa = (2i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1]);
                let sb = (2i32).wrapping_mul(p.samples_b[0]).wrapping_sub(p.samples_b[1]);
                p.samples_a[1] = p.samples_a[0];
                p.samples_b[1] = p.samples_b[0];

                let na = buf[i * 2    ].wrapping_add(apply_weight(p.weight_a, sa));
                let nb = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, sb));
                update_weight(&mut p.weight_a, p.delta, sa, buf[i * 2    ]);
                update_weight(&mut p.weight_b, p.delta, sb, buf[i * 2 + 1]);
                p.samples_a[0] = na; buf[i * 2    ] = na;
                p.samples_b[0] = nb; buf[i * 2 + 1] = nb;
            }
        }
        18 => {
            for i in 0..n {
                let sa = ((3i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1])) >> 1;
                let sb = ((3i32).wrapping_mul(p.samples_b[0]).wrapping_sub(p.samples_b[1])) >> 1;
                p.samples_a[1] = p.samples_a[0];
                p.samples_b[1] = p.samples_b[0];

                let na = buf[i * 2    ].wrapping_add(apply_weight(p.weight_a, sa));
                let nb = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, sb));
                update_weight(&mut p.weight_a, p.delta, sa, buf[i * 2    ]);
                update_weight(&mut p.weight_b, p.delta, sb, buf[i * 2 + 1]);
                p.samples_a[0] = na; buf[i * 2    ] = na;
                p.samples_b[0] = nb; buf[i * 2 + 1] = nb;
            }
        }
        -1 => {
            for i in 0..n {
                let sam = buf[i * 2].wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                update_weight_clip(&mut p.weight_a, p.delta, p.samples_a[0], buf[i * 2]);
                buf[i * 2] = sam;
                p.samples_a[0] = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, sam));
                update_weight_clip(&mut p.weight_b, p.delta, sam, buf[i * 2 + 1]);
                buf[i * 2 + 1] = p.samples_a[0];
            }
        }
        -2 => {
            for i in 0..n {
                let sam = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));
                update_weight_clip(&mut p.weight_b, p.delta, p.samples_b[0], buf[i * 2 + 1]);
                buf[i * 2 + 1] = sam;
                p.samples_b[0] = buf[i * 2].wrapping_add(apply_weight(p.weight_a, sam));
                update_weight_clip(&mut p.weight_a, p.delta, sam, buf[i * 2]);
                buf[i * 2] = p.samples_b[0];
            }
        }
        -3 => {
            for i in 0..n {
                let sam_a = buf[i * 2    ].wrapping_add(apply_weight(p.weight_a, p.samples_a[0]));
                update_weight_clip(&mut p.weight_a, p.delta, p.samples_a[0], buf[i * 2]);
                p.samples_a[0] = buf[i * 2 + 1].wrapping_add(apply_weight(p.weight_b, p.samples_b[0]));
                update_weight_clip(&mut p.weight_b, p.delta, p.samples_b[0], buf[i * 2 + 1]);
                buf[i * 2    ] = sam_a;
                buf[i * 2 + 1] = p.samples_a[0];
                p.samples_b[0] = sam_a;
            }
        }
        _ => {}
    }
}

pub fn decorr_mono_pass(p: &mut DecorrPass, buf: &mut [i32]) {
    let mut m = 0usize;

    match p.term {
        t if t > 0 && t <= MAX_TERM as i32 => {
            for x in buf.iter_mut() {
                let sam = p.samples_a[m];
                let k   = (m + t as usize) & (MAX_TERM - 1);
                let na  = x.wrapping_add(apply_weight(p.weight_a, sam));
                update_weight(&mut p.weight_a, p.delta, sam, *x);
                p.samples_a[k] = na;
                *x = na;
                m = (m + 1) & (MAX_TERM - 1);
            }
        }
        17 => {
            for x in buf.iter_mut() {
                let sa = (2i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1]);
                p.samples_a[1] = p.samples_a[0];
                let na = x.wrapping_add(apply_weight(p.weight_a, sa));
                update_weight(&mut p.weight_a, p.delta, sa, *x);
                p.samples_a[0] = na;
                *x = na;
            }
        }
        18 => {
            for x in buf.iter_mut() {
                let sa = ((3i32).wrapping_mul(p.samples_a[0]).wrapping_sub(p.samples_a[1])) >> 1;
                p.samples_a[1] = p.samples_a[0];
                let na = x.wrapping_add(apply_weight(p.weight_a, sa));
                update_weight(&mut p.weight_a, p.delta, sa, *x);
                p.samples_a[0] = na;
                *x = na;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// fixup_samples: apply shift, joint-stereo undo, int32 restoration
// ---------------------------------------------------------------------------

/// Undo joint-stereo decorrelation: `bptr[0] += (bptr[1] -= (bptr[0] >> 1))`.
/// Runs unconditionally after the decorrelation passes (for both integer and float
/// data), matching the placement in unpack.c's stereo decode loop rather than in
/// `fixup_samples()` (which for `FLOAT_DATA` returns before doing anything else).
pub fn undo_joint_stereo(buf: &mut [i32], flags: u32) {
    if (flags & MONO_DATA) == 0 && (flags & JOINT_STEREO) != 0 {
        let n = buf.len() / 2;
        for i in 0..n {
            let r_new = buf[i * 2 + 1].wrapping_sub(buf[i * 2] >> 1);
            buf[i * 2    ] = buf[i * 2].wrapping_add(r_new);
            buf[i * 2 + 1] = r_new;
        }
    }
}

/// Final integer fixup: INT32_DATA extra-bit restoration, hybrid-lossy clipping and
/// the residual precision shift. Not used for `FLOAT_DATA` blocks (see `floats::float_values`).
/// Port of the (non-float) tail of `fixup_samples()` in unpack.c.
pub fn fixup_samples(buf: &mut [i32], flags: u32, i32info: &Int32Info) {
    let is_hybrid_lossy = (flags & HYBRID_FLAG) != 0; // no .wvc support => always "lossy" here
    let mut shift = ((flags & SHIFT_MASK) >> SHIFT_LSB) as i32;

    // INT32_DATA: restore extra bits (for lossless 32-bit sources)
    if (flags & INT32_DATA) != 0 && i32info.sent_bits == 0 {
        let extra = i32info.zeros as i32 + i32info.ones as i32 + i32info.dups as i32;
        if extra != 0 {
            for s in buf.iter_mut() {
                *s <<= i32info.zeros + i32info.ones + i32info.dups;
                if i32info.ones != 0 {
                    *s |= ((1i32 << i32info.ones) - 1) << i32info.zeros;
                }
            }
        }
    }

    shift &= 0x1f;
    let shift = shift as u32;

    if is_hybrid_lossy {
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
}

// ---------------------------------------------------------------------------
// Main entry point: decode one v4/v5 block
// ---------------------------------------------------------------------------

pub fn unpack_samples_v4v5(
    flags:         u32,
    block_samples: u32,
    passes:        &mut [DecorrPass],
    ws:            &mut WordsState,
    i32info:       &Int32Info,
    float_info:    Option<&super::floats::FloatInfo>,
    wvx:           &[u8],
    audio:         &[u8],
) -> Option<Vec<i32>> {
    // WavPack's own encoder caps block size at 131072 samples (`--blocksize`); reject
    // anything wildly larger up front rather than risk an unbounded allocation or an
    // arithmetic overflow further down from a malformed/mutated `block_samples` field
    // (this decoder must never panic on untrusted network-stream input).
    if block_samples > MAX_BLOCK_SAMPLES {
        return None;
    }

    let is_mono = (flags & MONO_DATA) != 0;
    let mut bs  = Bits::new(audio);

    let mut buf = if (flags & HYBRID_FLAG) != 0 {
        get_words_hybrid(&mut bs, ws, flags, block_samples)?
    } else {
        get_words_lossless(&mut bs, ws, flags, block_samples)?
    };

    // Apply decorrelation passes in forward order
    for p in passes.iter_mut() {
        if is_mono {
            decorr_mono_pass(p, &mut buf);
        } else {
            decorr_stereo_pass(p, &mut buf);
        }
    }

    undo_joint_stereo(&mut buf, flags);

    if (flags & FLOAT_DATA) != 0 {
        wp_trace!("[float] wvx_len={} float_info_present={}", wvx.len(), float_info.is_some());
        let info = float_info.copied().unwrap_or_default();
        // The WVX "extension" bitstream carries a 4-byte CRC prefix (unused here; the
        // decoder never fails hard on a CRC mismatch, matching the hybrid-lossy path
        // which has no CRC at all) before the actual bit-packed correction data.
        if wvx.len() > 4 {
            let mut wvx_bits = Bits::new(&wvx[4..]);
            super::floats::float_values(&mut buf, &info, Some(&mut wvx_bits));
        } else {
            super::floats::float_values(&mut buf, &info, None);
        }
    } else {
        fixup_samples(&mut buf, flags, i32info);
    }

    Some(buf)
}
