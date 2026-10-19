// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A decoder of bzip2 streams, used to undo the bzlib `ContentCompression` of Matroska tracks.
//!
//! The stream format is that of the reference implementation, libbzip2 (`decompress.c`,
//! `huffman.c`, `bzlib_private.h`) by Julian Seward: a `BZh1`..`BZh9` header, blocks introduced by
//! the magic `0x314159265359`, and a trailer introduced by `0x177245385090` that carries the
//! combined CRC of the blocks. A block is, in order of decoding: Huffman coded MTF/RLE2 symbols,
//! an inverse Burrows-Wheeler transform, and an inverse RLE1.
//!
//! Deviation: blocks with the `randomised` flag, which were last produced by bzip2 0.9.0 and are
//! deprecated, are rejected instead of derandomised.

use symphonia_core::checksum::Crc32;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::Monitor;

/// The maximum size of a decompressed frame. No frame of an audio codec is anywhere close.
const MAX_OUTPUT: usize = 1 << 28;

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const END_MAGIC: u64 = 0x1772_4538_5090;

/// The maximum length of a Huffman code.
const MAX_CODE_LEN: usize = 20;
/// The number of symbols that are coded with one Huffman table before the selector advances.
const GROUP_SIZE: usize = 50;
/// The maximum number of selectors that is used (libbzip2 `BZ_MAX_SELECTORS`). More are read and
/// discarded.
const MAX_SELECTORS: usize = 18002;

const RUNA: u16 = 0;
const RUNB: u16 = 1;

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl BitReader<'_> {
    /// Read `n <= 32` bits, most significant first.
    fn bits(&mut self, n: u32) -> Result<u32> {
        while self.cnt < n {
            let Some(&byte) = self.data.get(self.pos)
            else {
                return decode_error("mkv: truncated bzip2 stream");
            };
            self.pos += 1;
            self.buf = (self.buf << 8) | u64::from(byte);
            self.cnt += 8;
        }
        self.cnt -= n;
        Ok(((self.buf >> self.cnt) & ((1u64 << n) - 1)) as u32)
    }

    fn bit(&mut self) -> Result<bool> {
        Ok(self.bits(1)? == 1)
    }

    /// `true` if the stream has no more bytes (a partial byte of padding may remain).
    fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Discard the bits up to the next byte boundary.
    fn align(&mut self) {
        self.cnt -= self.cnt % 8;
    }
}

/// A canonical Huffman code (codes of the same length are assigned in symbol order).
struct Huffman {
    count: [u16; MAX_CODE_LEN + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut count = [0u16; MAX_CODE_LEN + 1];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }

        let mut offs = [0u16; MAX_CODE_LEN + 2];
        for l in 1..=MAX_CODE_LEN {
            offs[l + 1] = offs[l] + count[l];
        }

        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            symbols[usize::from(offs[usize::from(l)])] = sym as u16;
            offs[usize::from(l)] += 1;
        }

        Self { count, symbols }
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;

        for len in 1..=MAX_CODE_LEN {
            code |= i32::from(br.bit()?);
            let count = i32::from(self.count[len]);
            if code - count < first {
                return match self.symbols.get((index + (code - first)) as usize) {
                    Some(&sym) => Ok(sym),
                    None => decode_error("mkv: invalid bzip2 huffman symbol"),
                };
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }

        decode_error("mkv: invalid bzip2 huffman code")
    }
}

/// Decompress a bzip2 stream.
pub(crate) fn bzip2_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut br = BitReader { data, pos: 0, buf: 0, cnt: 0 };

    if br.bits(8)? != u32::from(b'B') || br.bits(8)? != u32::from(b'Z') || br.bits(8)? != 0x68 {
        return decode_error("mkv: invalid bzip2 header");
    }

    let level = br.bits(8)?;
    if !(u32::from(b'1')..=u32::from(b'9')).contains(&level) {
        return decode_error("mkv: invalid bzip2 block size");
    }
    let max_block = (level - u32::from(b'0')) as usize * 100_000;

    let mut out = Vec::with_capacity(data.len().saturating_mul(3));
    let mut tt = Vec::new();
    let mut combined_crc = 0u32;

    loop {
        let magic = (u64::from(br.bits(24)?) << 24) | u64::from(br.bits(24)?);

        if magic == END_MAGIC {
            if br.bits(32)? != combined_crc {
                return decode_error("mkv: bzip2 stream crc mismatch");
            }

            // Concatenated streams (each padded to a byte) are decoded as one.
            br.align();
            br.cnt = 0;
            if br.at_end() {
                return Ok(out);
            }
            if br.bits(8)? != u32::from(b'B')
                || br.bits(8)? != u32::from(b'Z')
                || br.bits(8)? != 0x68
            {
                return decode_error("mkv: trailing data after bzip2 stream");
            }
            let level = br.bits(8)?;
            if !(u32::from(b'1')..=u32::from(b'9')).contains(&level) {
                return decode_error("mkv: invalid bzip2 block size");
            }
            // The block size of later streams is only an upper bound for validation.
            let _ = level;
            combined_crc = 0;
            continue;
        }

        if magic != BLOCK_MAGIC {
            return decode_error("mkv: invalid bzip2 block magic");
        }

        let block_crc = br.bits(32)?;
        let block_start = out.len();
        decode_block(&mut br, max_block, &mut tt, &mut out)?;

        let mut crc = Crc32::new(0xffff_ffff);
        crc.process_buf_bytes(&out[block_start..]);
        let crc = !crc.crc();

        if crc != block_crc {
            return decode_error("mkv: bzip2 block crc mismatch");
        }
        combined_crc = combined_crc.rotate_left(1) ^ crc;
    }
}

/// Decode a block (after its magic and CRC), appending the original bytes to `out`.
fn decode_block(
    br: &mut BitReader<'_>,
    max_block: usize,
    tt: &mut Vec<u32>,
    out: &mut Vec<u8>,
) -> Result<()> {
    if br.bit()? {
        return decode_error("mkv: randomised bzip2 blocks are not supported");
    }

    let orig_ptr = br.bits(24)? as usize;

    // The symbol map: the bytes that occur in the block, in two levels of bit sets.
    let used_groups = br.bits(16)?;
    let mut seq_to_unseq = [0u8; 256];
    let mut n_used = 0usize;
    for g in 0..16 {
        if used_groups & (0x8000 >> g) != 0 {
            let bits = br.bits(16)?;
            for k in 0..16 {
                if bits & (0x8000 >> k) != 0 {
                    seq_to_unseq[n_used] = (g * 16 + k) as u8;
                    n_used += 1;
                }
            }
        }
    }
    if n_used == 0 {
        return decode_error("mkv: bzip2 block uses no symbols");
    }

    // The symbols are: RUNA, RUNB, the MTF indices 1..n_used-1, and the end of block.
    let alpha_size = n_used + 2;
    let eob = (n_used + 1) as u16;

    let n_groups = br.bits(3)? as usize;
    if !(2..=6).contains(&n_groups) {
        return decode_error("mkv: invalid bzip2 table count");
    }

    let n_selectors = br.bits(15)? as usize;
    if n_selectors == 0 {
        return decode_error("mkv: invalid bzip2 selector count");
    }

    // Selectors are MTF coded, with unary indices.
    let mut mtf_groups = [0u8, 1, 2, 3, 4, 5];
    let mut selectors = Vec::with_capacity(n_selectors.min(MAX_SELECTORS));
    for _ in 0..n_selectors {
        let mut j = 0;
        while br.bit()? {
            j += 1;
            if j >= n_groups {
                return decode_error("mkv: invalid bzip2 selector");
            }
        }
        let v = mtf_groups[j];
        mtf_groups.copy_within(0..j, 1);
        mtf_groups[0] = v;
        if selectors.len() < MAX_SELECTORS {
            selectors.push(v);
        }
    }

    // The code lengths of each table, delta coded.
    let mut tables = Vec::with_capacity(n_groups);
    for _ in 0..n_groups {
        let mut lengths = vec![0u8; alpha_size];
        let mut curr = br.bits(5)? as i32;
        for len in lengths.iter_mut() {
            loop {
                if !(1..=MAX_CODE_LEN as i32).contains(&curr) {
                    return decode_error("mkv: invalid bzip2 code length");
                }
                if !br.bit()? {
                    break;
                }
                curr += if br.bit()? { -1 } else { 1 };
            }
            *len = curr as u8;
        }
        tables.push(Huffman::new(&lengths));
    }

    // Decode the MTF/RLE2 symbols into the block.
    tt.clear();
    let mut counts = [0u32; 256];
    let mut mtf = seq_to_unseq;
    let mut run = 0usize;
    let mut run_bit = 1usize;
    let mut sel_idx = 0;
    let mut left = 0;
    let mut table = &tables[0];

    loop {
        if left == 0 {
            let Some(&sel) = selectors.get(sel_idx)
            else {
                return decode_error("mkv: bzip2 selectors exhausted");
            };
            table = &tables[usize::from(sel)];
            sel_idx += 1;
            left = GROUP_SIZE;
        }
        left -= 1;

        let sym = table.decode(br)?;

        if sym == RUNA || sym == RUNB {
            // Bijective base-2 run length of the byte at the front of the MTF list.
            if run_bit > max_block {
                return decode_error("mkv: bzip2 run too long");
            }
            run += run_bit << sym;
            run_bit <<= 1;
            continue;
        }

        if run > 0 {
            if tt.len() + run > max_block {
                return decode_error("mkv: bzip2 block too large");
            }
            let byte = mtf[0];
            counts[usize::from(byte)] += run as u32;
            tt.resize(tt.len() + run, u32::from(byte));
            run = 0;
            run_bit = 1;
        }

        if sym == eob {
            break;
        }

        if tt.len() >= max_block {
            return decode_error("mkv: bzip2 block too large");
        }

        let idx = usize::from(sym) - 1;
        let byte = mtf[idx];
        mtf.copy_within(0..idx, 1);
        mtf[0] = byte;
        counts[usize::from(byte)] += 1;
        tt.push(u32::from(byte));
    }

    let n = tt.len();
    if n == 0 {
        return Ok(());
    }
    if orig_ptr >= n {
        return decode_error("mkv: invalid bzip2 origin pointer");
    }

    // Inverse BWT: link every position to its successor in the upper 24 bits.
    let mut cftab = [0u32; 256];
    let mut sum = 0u32;
    for (c, &cnt) in cftab.iter_mut().zip(counts.iter()) {
        *c = sum;
        sum += cnt;
    }
    for i in 0..n {
        let b = usize::from(tt[i] as u8);
        let slot = cftab[b] as usize;
        tt[slot] |= (i as u32) << 8;
        cftab[b] += 1;
    }

    // Inverse RLE1: after four equal bytes, the next byte is the number of further repeats.
    let mut pos = tt[orig_ptr] >> 8;
    let mut prev = None;
    let mut same = 0u32;
    let mut i = 0;
    while i < n {
        let entry = tt[pos as usize];
        let byte = entry as u8;
        pos = entry >> 8;
        i += 1;

        if same == 4 {
            let extra = usize::from(byte);
            if out.len() + extra > MAX_OUTPUT {
                return decode_error("mkv: bzip2 output too large");
            }
            let last = prev.unwrap_or(0);
            out.resize(out.len() + extra, last);
            same = 0;
            prev = None;
            continue;
        }

        if prev == Some(byte) {
            same += 1;
        }
        else {
            same = 1;
            prev = Some(byte);
        }

        if out.len() >= MAX_OUTPUT {
            return decode_error("mkv: bzip2 output too large");
        }
        out.push(byte);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bzip2_decompress;

    // Produced by Python's bz2.compress(data, 9), a libbzip2 binding.
    const HELLO: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0xe5, 0xb5, 0xf3, 0x09, 0x00,
        0x00, 0x04, 0x51, 0x00, 0x00, 0x10, 0x40, 0x00, 0x02, 0x44, 0xa0, 0x00, 0x21, 0xb5, 0x18,
        0x0c, 0x02, 0x90, 0x69, 0xc2, 0xa3, 0x0b, 0xb9, 0x22, 0x9c, 0x28, 0x48, 0x72, 0xda, 0xf9,
        0x84, 0x80,
    ];

    #[test]
    fn verify_decodes_short_stream() {
        assert_eq!(bzip2_decompress(HELLO).unwrap(), b"hello hello hello\n");
    }

    // bz2.compress(data, 1) of the data built by `runs_data`.
    const RUNS: &[u8] = &[
        0x42, 0x5a, 0x68, 0x31, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0xca, 0x7f, 0x8b, 0xd0, 0x00,
        0x00, 0x0a, 0xff, 0xff, 0xc8, 0x40, 0x40, 0x40, 0x42, 0x42, 0x02, 0x02, 0x02, 0x12, 0x10,
        0x10, 0x38, 0x10, 0xb0, 0x90, 0x80, 0x80, 0x84, 0x84, 0x04, 0x04, 0x04, 0x24, 0x20, 0x20,
        0x20, 0x21, 0x01, 0x81, 0x01, 0x08, 0x20, 0x00, 0x54, 0x63, 0x46, 0x41, 0xa6, 0x40, 0x34,
        0x18, 0x8d, 0x00, 0xc8, 0x60, 0x8d, 0x50, 0x06, 0x40, 0x00, 0x68, 0xd0, 0x00, 0x68, 0x69,
        0x53, 0x16, 0xf5, 0xda, 0xbd, 0xf9, 0xd7, 0xcc, 0x78, 0x21, 0x8a, 0x39, 0x25, 0x58, 0x52,
        0x08, 0x82, 0xd2, 0x01, 0x15, 0x69, 0x22, 0x05, 0x6a, 0x10, 0x0d, 0x43, 0x22, 0xeb, 0xef,
        0x07, 0x48, 0xf4, 0xd5, 0x5b, 0x6f, 0xc7, 0x3d, 0x77, 0xe2, 0xd9, 0xef, 0xdf, 0xa5, 0xae,
        0x8a, 0x1c, 0x9e, 0x2e, 0xe4, 0x8a, 0x70, 0xa1, 0x21, 0x94, 0xff, 0x17, 0xa0,
    ];

    fn runs_data() -> Vec<u8> {
        let mut d = vec![b'a'; 1000];
        d.extend(b"bc".repeat(20));
        d.extend([b'z'; 4]);
        d.extend([b'q'; 259]);
        d.extend((0..40u32).map(|i| ((i * 37) % 251) as u8));
        d
    }

    #[test]
    fn verify_decodes_runs() {
        assert_eq!(bzip2_decompress(RUNS).unwrap(), runs_data());
    }

    #[test]
    fn verify_rejects_corruption() {
        let mut bad = HELLO.to_vec();
        // A bit of the stored block CRC.
        bad[10] ^= 0x10;
        assert!(bzip2_decompress(&bad).is_err());
        assert!(bzip2_decompress(&HELLO[..20]).is_err());
        assert!(bzip2_decompress(b"BZx9").is_err());
    }
}
