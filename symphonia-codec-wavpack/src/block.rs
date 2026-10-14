// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WavPack v4/v5 block layout: the 32-byte block header and the sub-blocks (metadata) that follow
//! it, shared by the demuxer and the decoder.
//!
//! Ported from `wavpack.h`, `open_utils.c` (`read_next_header()`, `read_metadata_buff()`,
//! `WavpackVerifySingleBlock()`) of the WavPack reference implementation
//! (<https://github.com/dbry/WavPack>, BSD licence).

/// The size of the block header (`WavpackHeader`) in bytes.
pub(crate) const HEADER_LEN: usize = 32;

/// The magic number at the start of every block.
pub(crate) const BLOCK_MARKER: [u8; 4] = *b"wvpk";

/// The lowest stream version the reference library decodes (`MIN_STREAM_VERS`).
pub(crate) const MIN_STREAM_VERS: u16 = 0x402;
/// The highest stream version the reference library decodes (`MAX_STREAM_VERS`).
pub(crate) const MAX_STREAM_VERS: u16 = 0x410;

// Block header flags (wavpack.h).
pub(crate) const BYTES_STORED: u32 = 3;
pub(crate) const MONO_FLAG: u32 = 0x0000_0004;
pub(crate) const HYBRID_FLAG: u32 = 0x0000_0008;
pub(crate) const JOINT_STEREO: u32 = 0x0000_0010;
pub(crate) const CROSS_DECORR: u32 = 0x0000_0020;
pub(crate) const HYBRID_SHAPE: u32 = 0x0000_0040;
pub(crate) const FLOAT_DATA: u32 = 0x0000_0080;
pub(crate) const INT32_DATA: u32 = 0x0000_0100;
pub(crate) const HYBRID_BITRATE: u32 = 0x0000_0200;
pub(crate) const HYBRID_BALANCE: u32 = 0x0000_0400;
pub(crate) const INITIAL_BLOCK: u32 = 0x0000_0800;
pub(crate) const FINAL_BLOCK: u32 = 0x0000_1000;
pub(crate) const SHIFT_LSB: u32 = 13;
pub(crate) const SHIFT_MASK: u32 = 0x1f << SHIFT_LSB;
pub(crate) const MAG_LSB: u32 = 18;
pub(crate) const MAG_MASK: u32 = 0x1f << MAG_LSB;
pub(crate) const SRATE_LSB: u32 = 23;
pub(crate) const SRATE_MASK: u32 = 0xf << SRATE_LSB;
pub(crate) const FALSE_STEREO: u32 = 0x4000_0000;
pub(crate) const NEW_SHAPING: u32 = 0x2000_0000;
pub(crate) const MONO_DATA: u32 = MONO_FLAG | FALSE_STEREO;
pub(crate) const HAS_CHECKSUM: u32 = 0x1000_0000;
pub(crate) const DSD_FLAG: u32 = 0x8000_0000;

// Sub-block IDs (wavpack.h).
pub(crate) const ID_UNIQUE: u8 = 0x3f;
pub(crate) const ID_OPTIONAL_DATA: u8 = 0x20;
pub(crate) const ID_ODD_SIZE: u8 = 0x40;
pub(crate) const ID_LARGE: u8 = 0x80;

pub(crate) const ID_DECORR_TERMS: u8 = 0x2;
pub(crate) const ID_DECORR_WEIGHTS: u8 = 0x3;
pub(crate) const ID_DECORR_SAMPLES: u8 = 0x4;
pub(crate) const ID_ENTROPY_VARS: u8 = 0x5;
pub(crate) const ID_HYBRID_PROFILE: u8 = 0x6;
pub(crate) const ID_SHAPING_WEIGHTS: u8 = 0x7;
pub(crate) const ID_FLOAT_INFO: u8 = 0x8;
pub(crate) const ID_INT32_INFO: u8 = 0x9;
pub(crate) const ID_WV_BITSTREAM: u8 = 0xa;
pub(crate) const ID_WVC_BITSTREAM: u8 = 0xb;
pub(crate) const ID_WVX_BITSTREAM: u8 = 0xc;
pub(crate) const ID_CHANNEL_INFO: u8 = 0xd;
pub(crate) const ID_DSD_BLOCK: u8 = 0xe;
pub(crate) const ID_RIFF_HEADER: u8 = ID_OPTIONAL_DATA | 0x1;
pub(crate) const ID_RIFF_TRAILER: u8 = ID_OPTIONAL_DATA | 0x2;
/// Encoder configuration flags (`CONFIG_*`).
pub(crate) const ID_CONFIG_BLOCK: u8 = ID_OPTIONAL_DATA | 0x5;
pub(crate) const ID_MD5_CHECKSUM: u8 = ID_OPTIONAL_DATA | 0x6;
/// The MD5 checksum of a stream of an alternate type, such as DSD (`ID_ALT_MD5_CHECKSUM`).
pub(crate) const ID_ALT_MD5_CHECKSUM: u8 = ID_OPTIONAL_DATA | 0x9;
/// The file format, the source format flags ("qmode") and the channel layout of the source.
pub(crate) const ID_NEW_CONFIG_BLOCK: u8 = ID_OPTIONAL_DATA | 0xa;
pub(crate) const ID_SAMPLE_RATE: u8 = ID_OPTIONAL_DATA | 0x7;
pub(crate) const ID_WVX_NEW_BITSTREAM: u8 = ID_OPTIONAL_DATA | ID_WVX_BITSTREAM;
pub(crate) const ID_BLOCK_CHECKSUM: u8 = ID_OPTIONAL_DATA | 0xf;

/// The standard sample rates addressed by the sample rate field of the block flags.
pub(crate) const SAMPLE_RATES: [u32; 15] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    192000,
];

/// The decoded block header (`WavpackHeader`).
#[derive(Copy, Clone, Debug)]
pub(crate) struct BlockHeader {
    /// The number of bytes of the block after the first 8 (the marker and this field).
    pub ck_size: u32,
    /// The 40-bit index of the first sample of the block.
    pub block_index: u64,
    /// The low 32 bits of the total number of samples of the file. All ones is "unknown".
    pub total_samples: u32,
    /// The high 8 bits of the total number of samples of the file.
    pub total_samples_u8: u8,
    /// The number of samples (per channel) in the block.
    pub block_samples: u32,
    pub flags: u32,
    pub crc: u32,
}

impl BlockHeader {
    /// Returns `true` if `bytes` (at least `HEADER_LEN` of them) are a plausible block header.
    /// Port of the validity test in `read_next_header()`.
    pub fn is_plausible(bytes: &[u8]) -> bool {
        if bytes.len() < HEADER_LEN || bytes[..4] != BLOCK_MARKER {
            return false;
        }
        let ck_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        let block_samples = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);

        ck_size & 1 == 0
            && ck_size < 1 << 20
            && ck_size >= 24
            && (version >> 8) == 4
            && (MIN_STREAM_VERS..=MAX_STREAM_VERS).contains(&version)
            && block_samples < 0x30000
    }

    /// Decode the header from the first `HEADER_LEN` bytes of `bytes`.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN || bytes[..4] != BLOCK_MARKER {
            return None;
        }
        let u32_at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

        Some(BlockHeader {
            ck_size: u32_at(4),
            block_index: u64::from(u32_at(16)) | (u64::from(bytes[10]) << 32),
            total_samples: u32_at(12),
            total_samples_u8: bytes[11],
            block_samples: u32_at(20),
            flags: u32_at(24),
            crc: u32_at(28),
        })
    }

    /// The total number of samples of the file, or `None` if it is unknown.
    pub fn file_total_samples(&self) -> Option<u64> {
        // `GET_TOTAL_SAMPLES()` in wavpack.h: -1 if the low 32 bits are all ones, otherwise the
        // 40-bit value with the high byte compensated for.
        if self.total_samples == u32::MAX {
            None
        }
        else {
            let hi = u64::from(self.total_samples_u8);
            Some(u64::from(self.total_samples) + (hi << 32) - hi)
        }
    }

    pub fn is_mono(&self) -> bool {
        self.flags & MONO_FLAG != 0
    }

    pub fn is_dsd(&self) -> bool {
        self.flags & DSD_FLAG != 0
    }

    pub fn is_float(&self) -> bool {
        self.flags & FLOAT_DATA != 0
    }

    pub fn bytes_per_sample(&self) -> u32 {
        (self.flags & BYTES_STORED) + 1
    }

    /// The number of bits of the original samples that are not shifted out.
    pub fn bits_per_sample(&self) -> u32 {
        (self.bytes_per_sample() * 8).saturating_sub((self.flags & SHIFT_MASK) >> SHIFT_LSB).max(1)
    }

    /// The sample rate of the sample rate field of the flags, if it is a standard one.
    pub fn standard_sample_rate(&self) -> Option<u32> {
        SAMPLE_RATES.get(((self.flags & SRATE_MASK) >> SRATE_LSB) as usize).copied()
    }
}

/// A sub-block (metadata block) of a block.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SubBlock<'a> {
    /// The function ID, without the "large" and "odd size" flags (it includes the "optional
    /// data" bit).
    pub id: u8,
    /// The data, without the padding byte of an odd sized sub-block.
    pub data: &'a [u8],
}

/// An iterator over the sub-blocks in the body of a block (the bytes after the header).
/// Port of `read_metadata_buff()` in open_utils.c: the iteration ends at the first malformed
/// sub-block, which can be detected with `is_well_formed()`.
#[derive(Clone)]
pub(crate) struct SubBlocks<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> SubBlocks<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        SubBlocks { body, pos: 0 }
    }

    /// Returns `true` if the whole body is a sequence of well-formed sub-blocks.
    pub fn is_well_formed(&self) -> bool {
        let mut it = self.clone();
        while it.next().is_some() {}
        it.pos == it.body.len()
    }
}

impl<'a> Iterator for SubBlocks<'a> {
    type Item = SubBlock<'a>;

    fn next(&mut self) -> Option<SubBlock<'a>> {
        let rest = &self.body[self.pos..];
        if rest.len() < 2 {
            return None;
        }

        let mut id = rest[0];
        let mut byte_len = (rest[1] as usize) << 1;
        let mut hdr: usize = 2;

        if id & ID_LARGE != 0 {
            if rest.len() < 4 {
                return None;
            }
            byte_len += ((rest[2] as usize) << 9) + ((rest[3] as usize) << 17);
            hdr = 4;
            id &= !ID_LARGE;
        }

        let mut data_len = byte_len;
        if id & ID_ODD_SIZE != 0 {
            // Odd size with no data makes no sense.
            if byte_len == 0 {
                return None;
            }
            data_len -= 1;
            id &= !ID_ODD_SIZE;
        }

        // The data is padded to a whole number of 16-bit words.
        let end = hdr.checked_add(data_len + (data_len & 1))?;
        if rest.len() < end {
            return None;
        }

        self.pos += end;
        Some(SubBlock { id, data: &rest[hdr..hdr + data_len] })
    }
}

/// Verify the integrity of a complete block (header and body): the sub-blocks must be well
/// formed and, if there is a block checksum sub-block, the checksum must match. Port of
/// `WavpackVerifySingleBlock()` in open_utils.c (the `HAS_CHECKSUM` flag is not required to be
/// matched by a checksum, and the checksum is only checked if the header says that there is one:
/// Matroska strips them).
pub(crate) fn verify_block(block: &[u8]) -> bool {
    if block.len() < HEADER_LEN || block[..4] != BLOCK_MARKER {
        return false;
    }

    let has_checksum =
        u32::from_le_bytes([block[24], block[25], block[26], block[27]]) & HAS_CHECKSUM != 0;

    let body = &block[HEADER_LEN..];
    let mut pos = 0usize;
    let mut checksum_passed = false;

    while body.len() - pos >= 2 {
        let id = body[pos];
        let mut byte_len = (body[pos + 1] as usize) << 1;
        let mut hdr: usize = 2;

        if id & ID_LARGE != 0 {
            if body.len() - pos < 4 {
                return false;
            }
            byte_len += ((body[pos + 2] as usize) << 9) + ((body[pos + 3] as usize) << 17);
            hdr = 4;
        }

        if body.len() - pos - hdr < byte_len {
            return false;
        }

        if has_checksum && id & ID_UNIQUE == ID_BLOCK_CHECKSUM {
            if id & ID_ODD_SIZE != 0 || !(2..=4).contains(&byte_len) {
                return false;
            }

            // The checksum is over everything up to the sub-block, in little-endian 16-bit
            // words.
            // (`wcount` in `WavpackVerifySingleBlock()` ends at 2 bytes before the data.)
            let covered = &block[..HEADER_LEN + pos + hdr - 2];
            let mut csum = u32::MAX;
            for w in covered.chunks_exact(2) {
                csum = csum.wrapping_mul(3).wrapping_add(u32::from(w[0]) + (u32::from(w[1]) << 8));
            }

            let stored = &body[pos + hdr..pos + hdr + byte_len];
            let ok = if byte_len == 4 {
                stored == csum.to_le_bytes()
            }
            else {
                csum ^= csum >> 16;
                stored == &csum.to_le_bytes()[..2]
            };

            if !ok {
                return false;
            }
            checksum_passed = true;
        }

        pos += hdr + byte_len;
    }

    pos == body.len() && (!has_checksum || checksum_passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ck_size: u32, flags: u32) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(&BLOCK_MARKER);
        h[4..8].copy_from_slice(&ck_size.to_le_bytes());
        h[8..10].copy_from_slice(&0x410u16.to_le_bytes());
        h[20..24].copy_from_slice(&1000u32.to_le_bytes());
        h[24..28].copy_from_slice(&flags.to_le_bytes());
        h
    }

    #[test]
    fn verify_sub_block_iteration() {
        // A normal sub-block, an odd sized one (with its padding byte), and a large one.
        let mut body = vec![0x02, 0x02, 1, 2, 3, 4];
        body.extend([0x40 | 0x0a, 0x02, 5, 6, 7, 0]);
        body.extend([0x80 | 0x0b, 0x01, 0x00, 0x00, 8, 9]);

        let blocks: Vec<_> = SubBlocks::new(&body).collect();
        assert_eq!(blocks.len(), 3);
        assert_eq!((blocks[0].id, blocks[0].data), (0x02, &[1u8, 2, 3, 4][..]));
        assert_eq!((blocks[1].id, blocks[1].data), (0x0a, &[5u8, 6, 7][..]));
        assert_eq!((blocks[2].id, blocks[2].data), (0x0b, &[8u8, 9][..]));
        assert!(SubBlocks::new(&body).is_well_formed());

        // Truncated and odd-sized-with-no-data sub-blocks are malformed.
        assert!(!SubBlocks::new(&body[..body.len() - 1]).is_well_formed());
        assert!(!SubBlocks::new(&[0x40 | 0x0a, 0x00]).is_well_formed());
        assert!(!SubBlocks::new(&[0x0a]).is_well_formed());
    }

    #[test]
    fn verify_block_checksum() {
        for (size, ck) in [(4usize, 4u32), (2, 2)] {
            let body = [0x02u8, 0x02, 1, 2, 3, 4];
            let mut block = header(24 + body.len() as u32 + 2 + size as u32, HAS_CHECKSUM).to_vec();
            block.extend(body);

            // The checksum covers the block up to the sub-block of the checksum.
            let mut csum = u32::MAX;
            for w in block.chunks_exact(2) {
                csum = csum.wrapping_mul(3).wrapping_add(u32::from(w[0]) + (u32::from(w[1]) << 8));
            }
            block.extend([ID_BLOCK_CHECKSUM, (ck / 2) as u8]);
            if size == 2 {
                csum ^= csum >> 16;
            }
            block.extend(&csum.to_le_bytes()[..size]);

            assert!(verify_block(&block), "{size}-byte checksum");

            // A block that says it has a checksum must have a sub-block with it.
            assert!(!verify_block(&block[..block.len() - 2 - size]));

            // Any change to the covered bytes is detected.
            let mut bad = block.clone();
            bad[HEADER_LEN + 2] ^= 1;
            assert!(!verify_block(&bad));
        }
    }

    #[test]
    fn verify_plausible_headers() {
        assert!(BlockHeader::is_plausible(&header(100, 0)));
        // Odd or tiny sizes, bad versions and huge block sample counts are not blocks.
        assert!(!BlockHeader::is_plausible(&header(101, 0)));
        assert!(!BlockHeader::is_plausible(&header(8, 0)));
        let mut h = header(100, 0);
        h[8] = 0x01;
        assert!(!BlockHeader::is_plausible(&h));
        let mut h = header(100, 0);
        h[22] = 3;
        assert!(!BlockHeader::is_plausible(&h));
    }
}
