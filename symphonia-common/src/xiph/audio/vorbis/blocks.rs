// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing of the block sizes of a Vorbis stream.
//!
//! The decoded duration of a Vorbis audio packet depends on the block size of the packet and of
//! the packet that precedes it. The block size of a packet is selected by its mode, and the
//! block flag of each mode is stored in the setup header. A container that does not store the
//! duration of packets (e.g., Matroska) must therefore read the identification and setup headers
//! to derive an exact timeline.
//!
//! The setup header is parsed only as far as is required to reach the modes: codebooks, floors,
//! residues and mappings are skipped over, not interpreted.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderRtl, BufReader, ReadBitsRtl, ReadBytes};

/// The identification header packet size.
const IDENTIFICATION_HEADER_SIZE: usize = 30;

/// The packet type for an identification header.
const PACKET_TYPE_IDENTIFICATION: u8 = 1;
/// The packet type for a setup header.
const PACKET_TYPE_SETUP: u8 = 5;

/// The common header packet signature.
const HEADER_PACKET_SIGNATURE: &[u8] = b"vorbis";

/// The minimum block size (64) expressed as a power-of-2 exponent.
const BLOCKSIZE_MIN_EXP: u8 = 6;
/// The maximum block size (8192) expressed as a power-of-2 exponent.
const BLOCKSIZE_MAX_EXP: u8 = 13;

/// The block sizes of a Vorbis stream, and the block size selected by each mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VorbisBlocks {
    /// The short block size, as a power-of-2 exponent.
    pub bs0_exp: u8,
    /// The long block size, as a power-of-2 exponent.
    pub bs1_exp: u8,
    /// The number of modes. In the range `1..=64`.
    pub num_modes: u8,
    /// The block flag of each mode, mode `n` being bit `n`. A set bit selects the long block.
    pub mode_block_flags: u64,
}

impl VorbisBlocks {
    /// Parse the block sizes from the identification and setup header packets of a stream.
    pub fn from_headers(ident: &[u8], setup: &[u8]) -> Result<Self> {
        let (n_channels, bs0_exp, bs1_exp) = read_ident_header(ident)?;
        let (num_modes, mode_block_flags) = read_setup_modes(setup, n_channels)?;

        Ok(VorbisBlocks { bs0_exp, bs1_exp, num_modes, mode_block_flags })
    }

    /// Get the block size, as a power-of-2 exponent, of an audio packet.
    ///
    /// Returns `None` if the packet is not an audio packet, or if it selects an invalid mode.
    pub fn packet_block_exp(&self, packet: &[u8]) -> Option<u8> {
        // The first bit of the packet is the packet type, it must be 0 for an audio packet,
        // followed by the mode number, packed least-significant-bit first.
        let &first = packet.first()?;

        if first & 1 != 0 {
            return None;
        }

        // The number of bits of the mode number is ilog(num_modes - 1). It's never more than 6, so
        // the packet type and mode number are always within the first byte.
        let mode_num_bits = ilog(u32::from(self.num_modes) - 1);
        let mode = (first >> 1) & (((1u16 << mode_num_bits) - 1) as u8);

        if mode >= self.num_modes {
            return None;
        }

        if (self.mode_block_flags >> mode) & 1 == 1 {
            Some(self.bs1_exp)
        }
        else {
            Some(self.bs0_exp)
        }
    }

    /// Get the number of frames decoded from an audio packet of block size `cur_bs_exp`, that
    /// follows a packet of block size `prev_bs_exp`.
    pub fn packet_duration(prev_bs_exp: u8, cur_bs_exp: u8) -> u64 {
        ((1u64 << prev_bs_exp) >> 2) + ((1u64 << cur_bs_exp) >> 2)
    }
}

/// Read the identification header. Returns the number of channels, and the exponents of the short
/// and long block sizes.
fn read_ident_header(buf: &[u8]) -> Result<(u8, u8, u8)> {
    if buf.len() != IDENTIFICATION_HEADER_SIZE {
        return decode_error("vorbis: invalid identification header size");
    }

    let mut reader = BufReader::new(buf);

    if reader.read_u8()? != PACKET_TYPE_IDENTIFICATION {
        return decode_error("vorbis: invalid packet type for identification header");
    }

    let mut signature = [0; 6];
    reader.read_buf_exact(&mut signature)?;

    if signature != HEADER_PACKET_SIGNATURE {
        return decode_error("vorbis: invalid identification header signature");
    }

    if reader.read_u32()? != 0 {
        return unsupported_error("vorbis: only vorbis 1 is supported");
    }

    let n_channels = reader.read_u8()?;

    if n_channels == 0 {
        return decode_error("vorbis: number of channels cannot be 0");
    }

    // Sample rate, and the maximum, nominal, and minimum bitrates.
    reader.ignore_bytes(4 + 3 * 4)?;

    let block_sizes = reader.read_u8()?;

    let bs0_exp = block_sizes & 0x0f;
    let bs1_exp = block_sizes >> 4;

    if !(BLOCKSIZE_MIN_EXP..=BLOCKSIZE_MAX_EXP).contains(&bs0_exp)
        || !(BLOCKSIZE_MIN_EXP..=BLOCKSIZE_MAX_EXP).contains(&bs1_exp)
    {
        return decode_error("vorbis: block size out-of-bounds");
    }

    if bs0_exp > bs1_exp {
        return decode_error("vorbis: blocksize_0 exceeds blocksize_1");
    }

    if reader.read_u8()? != 1 {
        return decode_error("vorbis: identification header framing flag unset");
    }

    Ok((n_channels, bs0_exp, bs1_exp))
}

/// Read the setup header up to, and including, the modes. Returns the number of modes, and their
/// block flags.
fn read_setup_modes(buf: &[u8], n_channels: u8) -> Result<(u8, u64)> {
    let mut reader = BufReader::new(buf);

    if reader.read_u8()? != PACKET_TYPE_SETUP {
        return decode_error("vorbis: invalid packet type for setup header");
    }

    let mut signature = [0; 6];
    reader.read_buf_exact(&mut signature)?;

    if signature != HEADER_PACKET_SIGNATURE {
        return decode_error("vorbis: invalid setup header signature");
    }

    // The remainder of the setup header is read bitwise.
    let mut bs = BitReaderRtl::new(reader.read_buf_bytes_available_ref());

    skip_codebooks(&mut bs)?;
    skip_time_domain_transforms(&mut bs)?;
    skip_floors(&mut bs)?;
    skip_residues(&mut bs)?;
    skip_mappings(&mut bs, n_channels)?;

    // Read the modes.
    let num_modes = bs.read_bits_leq32(6)? + 1;
    let mut flags = 0u64;

    for mode in 0..num_modes {
        let block_flag = bs.read_bool()?;
        let window_type = bs.read_bits_leq32(16)?;
        let transform_type = bs.read_bits_leq32(16)?;
        let _mapping = bs.read_bits_leq32(8)?;

        // Only window type 0 and transform type 0 are allowed in Vorbis 1.
        if window_type != 0 || transform_type != 0 {
            return decode_error("vorbis: invalid window or transform type for mode");
        }

        flags |= u64::from(block_flag) << mode;
    }

    // Framing flag must be set.
    if !bs.read_bool()? {
        return decode_error("vorbis: setup header framing flag unset");
    }

    Ok((num_modes as u8, flags))
}

fn skip_codebooks(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    let count = bs.read_bits_leq32(8)? + 1;

    for _ in 0..count {
        skip_codebook(bs)?;
    }

    Ok(())
}

fn skip_codebook(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    // Verify the codebook synchronization word.
    if bs.read_bits_leq32(24)? != 0x564342 {
        return decode_error("vorbis: invalid codebook sync");
    }

    let dimensions = bs.read_bits_leq32(16)?;
    let entries = bs.read_bits_leq32(24)?;

    if bs.read_bool()? {
        // The codeword lengths are length ordered.
        let _first_length = bs.read_bits_leq32(5)?;

        let mut cur_entry = 0;

        while cur_entry < entries {
            let num_bits = ilog(entries - cur_entry);
            cur_entry += bs.read_bits_leq32(num_bits)?;

            if cur_entry > entries {
                return decode_error("vorbis: invalid codebook");
            }
        }
    }
    else if bs.read_bool()? {
        // Sparsely packed codeword lengths.
        for _ in 0..entries {
            if bs.read_bool()? {
                let _length = bs.read_bits_leq32(5)?;
            }
        }
    }
    else {
        bs.ignore_bits(entries * 5)?;
    }

    // The vector quantization lookup table.
    let lookup_type = bs.read_bits_leq32(4)?;

    match lookup_type {
        0 => (),
        1 | 2 => {
            let _min_value = bs.read_bits_leq32(32)?;
            let _delta_value = bs.read_bits_leq32(32)?;
            let value_bits = bs.read_bits_leq32(4)? + 1;
            let _sequence_p = bs.read_bool()?;

            let lookup_values = match lookup_type {
                1 => lookup1_values(entries, dimensions)?,
                _ => u64::from(entries) * u64::from(dimensions),
            };

            // Multiplicands.
            let bits = lookup_values * u64::from(value_bits);

            match u32::try_from(bits) {
                Ok(bits) => bs.ignore_bits(bits)?,
                Err(_) => return decode_error("vorbis: invalid codebook lookup table"),
            }
        }
        _ => return decode_error("vorbis: invalid codebook lookup type"),
    }

    Ok(())
}

fn skip_time_domain_transforms(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    let count = bs.read_bits_leq32(6)? + 1;

    for _ in 0..count {
        // All these values are placeholders and must be 0.
        if bs.read_bits_leq32(16)? != 0 {
            return decode_error("vorbis: invalid time domain transform");
        }
    }

    Ok(())
}

fn skip_floors(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    let count = bs.read_bits_leq32(6)? + 1;

    for _ in 0..count {
        match bs.read_bits_leq32(16)? {
            0 => skip_floor0(bs)?,
            1 => skip_floor1(bs)?,
            _ => return decode_error("vorbis: invalid floor type"),
        }
    }

    Ok(())
}

fn skip_floor0(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    // Order, rate, bark map size, amplitude bits, and amplitude offset.
    bs.ignore_bits(8 + 16 + 16 + 6 + 8)?;

    let number_of_books = bs.read_bits_leq32(4)? + 1;
    bs.ignore_bits(number_of_books * 8)?;

    Ok(())
}

fn skip_floor1(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    let partitions = bs.read_bits_leq32(5)? as usize;

    // The class of each partition (4 bits each), up to 32 partitions.
    let mut partition_classes = [0u8; 32];
    let mut class_dimensions = [0u32; 16];

    let mut max_class = 0;

    for class in &mut partition_classes[..partitions] {
        *class = bs.read_bits_leq32(4)? as u8;
        max_class = max_class.max(*class);
    }

    if partitions > 0 {
        for dimensions in &mut class_dimensions[..=usize::from(max_class)] {
            *dimensions = bs.read_bits_leq32(3)? + 1;

            let subclass_bits = bs.read_bits_leq32(2)?;

            if subclass_bits != 0 {
                let _main_book = bs.read_bits_leq32(8)?;
            }

            // The subclass books.
            bs.ignore_bits((1 << subclass_bits) * 8)?;
        }
    }

    let _multiplier = bs.read_bits_leq32(2)?;
    let range_bits = bs.read_bits_leq32(4)?;

    for &class in &partition_classes[..partitions] {
        bs.ignore_bits(class_dimensions[usize::from(class)] * range_bits)?;
    }

    Ok(())
}

fn skip_residues(bs: &mut BitReaderRtl<'_>) -> Result<()> {
    let count = bs.read_bits_leq32(6)? + 1;

    for _ in 0..count {
        let residue_type = bs.read_bits_leq32(16)?;

        if residue_type > 2 {
            return decode_error("vorbis: invalid residue type");
        }

        // Begin, end, and partition size.
        bs.ignore_bits(24 + 24 + 24)?;

        let classifications = bs.read_bits_leq32(6)? + 1;

        // The classification codebook.
        bs.ignore_bits(8)?;

        let mut num_books = 0;

        for _ in 0..classifications {
            let low_bits = bs.read_bits_leq32(3)?;
            let high_bits = if bs.read_bool()? { bs.read_bits_leq32(5)? } else { 0 };
            num_books += ((high_bits << 3) | low_bits).count_ones();
        }

        bs.ignore_bits(num_books * 8)?;
    }

    Ok(())
}

fn skip_mappings(bs: &mut BitReaderRtl<'_>, n_channels: u8) -> Result<()> {
    let count = bs.read_bits_leq32(6)? + 1;

    for _ in 0..count {
        // Only mapping type 0 is defined in Vorbis 1.
        if bs.read_bits_leq32(16)? != 0 {
            return decode_error("vorbis: invalid mapping type");
        }

        let num_submaps = if bs.read_bool()? { bs.read_bits_leq32(4)? + 1 } else { 1 };

        if bs.read_bool()? {
            let coupling_steps = bs.read_bits_leq32(8)? + 1;

            // The number of bits of the magnitude and angle channel numbers.
            let coupling_bits = ilog(u32::from(n_channels) - 1);

            for _ in 0..coupling_steps {
                let _magnitude_ch = bs.read_bits_leq32(coupling_bits)?;
                let _angle_ch = bs.read_bits_leq32(coupling_bits)?;
            }
        }

        if bs.read_bits_leq32(2)? != 0 {
            return decode_error("vorbis: reserved mapping bits non-zero");
        }

        // The submap each channel is multiplexed to.
        if num_submaps > 1 {
            bs.ignore_bits(u32::from(n_channels) * 4)?;
        }

        // Reserved, floor, and residue to use per submap.
        bs.ignore_bits(num_submaps * (8 + 8 + 8))?;
    }

    Ok(())
}

/// The number of bits required to represent `x` (0 for 0).
#[inline(always)]
fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// The greatest integer `v` such that `v ^ dimensions <= entries`.
fn lookup1_values(entries: u32, dimensions: u32) -> Result<u64> {
    if dimensions == 0 {
        return decode_error("vorbis: codebook has 0 dimensions");
    }

    let entries = u64::from(entries);

    // Is `v ^ dimensions` no more than `entries`? Saturating on overflow.
    let fits = |v: u64| {
        (0..dimensions)
            .try_fold(1u64, |acc, _| acc.checked_mul(v).filter(|&acc| acc <= entries))
            .is_some()
    };

    // The root is no more than `entries`, and, for 2 or more dimensions, no more than 2^12
    // (`entries` is a 24-bit value). Binary search for it.
    let (mut lo, mut hi) = (0u64, if dimensions == 1 { entries } else { 1 << 12 });

    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);

        if fits(mid) {
            lo = mid;
        }
        else {
            hi = mid - 1;
        }
    }

    Ok(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian-bit-order (least significant bit first) writer.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        len: usize,
    }

    impl BitWriter {
        fn put(&mut self, mut value: u64, bits: usize) {
            for _ in 0..bits {
                if self.len % 8 == 0 {
                    self.bytes.push(0);
                }
                *self.bytes.last_mut().unwrap() |= ((value & 1) as u8) << (self.len % 8);
                value >>= 1;
                self.len += 1;
            }
        }
    }

    pub(crate) fn ident_header(channels: u8, bs0_exp: u8, bs1_exp: u8) -> Vec<u8> {
        let mut h = vec![1];
        h.extend(b"vorbis");
        h.extend(0u32.to_le_bytes());
        h.push(channels);
        h.extend(44100u32.to_le_bytes());
        h.extend([0u8; 12]);
        h.push(bs0_exp | (bs1_exp << 4));
        h.push(1);
        h
    }

    /// A setup header with the given mode block flags and a variety of codebooks, floors,
    /// residues and mappings that must be skipped over.
    pub(crate) fn setup_header(channels: u8, mode_flags: &[bool]) -> Vec<u8> {
        let mut w = BitWriter::default();

        // Codebooks: sparse, dense, length ordered, with lookup types 0, 1, and 2.
        w.put(3, 8); // count - 1 = 3.

        // Sparse, 5 entries, lookup 0.
        w.put(0x564342, 24);
        w.put(2, 16);
        w.put(5, 24);
        w.put(0, 1); // Not ordered.
        w.put(1, 1); // Sparse.
        for used in [true, false, true, true, false] {
            w.put(u64::from(used), 1);
            if used {
                w.put(7, 5);
            }
        }
        w.put(0, 4);

        // Dense, 4 entries, lookup type 1 with 2 dimensions: sqrt(4) = 2 values of 3 bits.
        w.put(0x564342, 24);
        w.put(2, 16);
        w.put(4, 24);
        w.put(0, 1);
        w.put(0, 1);
        w.put(0, 4 * 5);
        w.put(1, 4);
        w.put(0, 32);
        w.put(0, 32);
        w.put(2, 4); // value_bits - 1.
        w.put(0, 1);
        w.put(0, 2 * 3);

        // Length ordered, 6 entries, lookup type 2 with 3 dimensions: 18 values of 2 bits.
        w.put(0x564342, 24);
        w.put(3, 16);
        w.put(6, 24);
        w.put(1, 1);
        w.put(4, 5);
        w.put(2, ilog(6) as usize); // 2 entries of length 5.
        w.put(4, ilog(4) as usize); // 4 entries of length 6.
        w.put(2, 4);
        w.put(0, 32);
        w.put(0, 32);
        w.put(1, 4);
        w.put(0, 1);
        w.put(0, 18 * 2);

        // Length ordered, 1 entry, no lookup.
        w.put(0x564342, 24);
        w.put(1, 16);
        w.put(1, 24);
        w.put(1, 1);
        w.put(0, 5);
        w.put(1, ilog(1) as usize);
        w.put(0, 4);

        // Time domain transforms: 1.
        w.put(0, 6);
        w.put(0, 16);

        // Floors: 2 (one of each type).
        w.put(1, 6);
        w.put(0, 16); // Floor 0.
        w.put(0, 8 + 16 + 16 + 6 + 8);
        w.put(1, 4); // 2 books.
        w.put(0, 16);
        w.put(1, 16); // Floor 1.
        w.put(3, 5); // 3 partitions.
        w.put(0, 4);
        w.put(1, 4);
        w.put(1, 4);
        // Classes 0 and 1.
        w.put(1, 3); // 2 dimensions.
        w.put(2, 2); // 2 subclass bits: 4 books.
        w.put(0, 8);
        w.put(0, 4 * 8);
        w.put(2, 3); // 3 dimensions.
        w.put(0, 2);
        w.put(0, 8);
        w.put(1, 2); // Multiplier.
        w.put(8, 4); // Range bits.
        w.put(0, (2 + 3 + 3) * 8);

        // Residues: 2.
        w.put(1, 6);
        for residue_type in [0, 2] {
            w.put(residue_type, 16);
            w.put(0, 72);
            w.put(2, 6); // 3 classifications.
            w.put(0, 8);
            w.put(0b101, 3); // Low bits: 2 books.
            w.put(0, 1);
            w.put(0b010, 3); // Low bits and high bits: 1 + 3 books.
            w.put(1, 1);
            w.put(0b00101, 5);
            w.put(0, 3);
            w.put(0, 1);
            w.put(0, 5 * 8);
        }

        // Mappings: 2.
        w.put(1, 6);
        // With 2 submaps and coupling.
        w.put(0, 16);
        w.put(1, 1);
        w.put(1, 4);
        w.put(1, 1);
        w.put(0, 8); // 1 coupling step.
        w.put(0, 2 * ilog(u32::from(channels) - 1) as usize);
        w.put(0, 2);
        w.put(0, usize::from(channels) * 4);
        w.put(0, 2 * 24);
        // With 1 submap, no coupling.
        w.put(0, 16);
        w.put(0, 1);
        w.put(0, 1);
        w.put(0, 2);
        w.put(0, 24);

        // Modes.
        w.put(mode_flags.len() as u64 - 1, 6);
        for &flag in mode_flags {
            w.put(u64::from(flag), 1);
            w.put(0, 16 + 16 + 8);
        }

        // Framing.
        w.put(1, 1);

        let mut h = vec![5];
        h.extend(b"vorbis");
        h.extend(w.bytes);
        h
    }

    #[test]
    fn parses_modes() {
        for channels in [1, 2, 6] {
            let blocks = VorbisBlocks::from_headers(
                &ident_header(channels, 8, 11),
                &setup_header(channels, &[false, true]),
            )
            .unwrap();

            assert_eq!(
                blocks,
                VorbisBlocks { bs0_exp: 8, bs1_exp: 11, num_modes: 2, mode_block_flags: 0b10 }
            );
        }

        let flags = [true, false, false, true, true, false, true, false, false];
        let blocks =
            VorbisBlocks::from_headers(&ident_header(2, 7, 12), &setup_header(2, &flags)).unwrap();
        assert_eq!(blocks.num_modes, 9);
        assert_eq!(blocks.mode_block_flags, 0b001011001);

        let blocks =
            VorbisBlocks::from_headers(&ident_header(2, 8, 11), &setup_header(2, &[true])).unwrap();
        assert_eq!(blocks.num_modes, 1);
        assert_eq!(blocks.mode_block_flags, 1);
    }

    #[test]
    fn rejects_malformed_headers() {
        let setup = setup_header(2, &[false, true]);
        let ident = ident_header(2, 8, 11);

        // Truncated setup header.
        assert!(VorbisBlocks::from_headers(&ident, &setup[..setup.len() - 12]).is_err());
        // Bad signatures and types.
        assert!(VorbisBlocks::from_headers(&ident, &[&[3u8][..], &setup[1..]].concat()).is_err());
        assert!(VorbisBlocks::from_headers(&[&[3u8][..], &ident[1..]].concat(), &setup).is_err());
        // Block size out of bounds, and block size 0 larger than block size 1.
        assert!(VorbisBlocks::from_headers(&ident_header(2, 5, 11), &setup).is_err());
        assert!(VorbisBlocks::from_headers(&ident_header(2, 11, 8), &setup).is_err());
        assert!(VorbisBlocks::from_headers(&ident_header(2, 8, 14), &setup).is_err());
        // Empty.
        assert!(VorbisBlocks::from_headers(&[], &[]).is_err());
    }

    #[test]
    fn packet_block_sizes() {
        let blocks =
            VorbisBlocks { bs0_exp: 8, bs1_exp: 11, num_modes: 3, mode_block_flags: 0b100 };

        // The mode number is 2 bits, and follows the packet type bit.
        assert_eq!(blocks.packet_block_exp(&[0b000]), Some(8));
        assert_eq!(blocks.packet_block_exp(&[0b010, 0xff]), Some(8));
        assert_eq!(blocks.packet_block_exp(&[0b100 | 0xf0]), Some(11));
        // Mode 3 does not exist.
        assert_eq!(blocks.packet_block_exp(&[0b110]), None);
        // Header packets (type bit set), and empty packets.
        assert_eq!(blocks.packet_block_exp(&[0x01]), None);
        assert_eq!(blocks.packet_block_exp(&[]), None);

        // A single mode has no mode number bits.
        let blocks = VorbisBlocks { bs0_exp: 8, bs1_exp: 11, num_modes: 1, mode_block_flags: 1 };
        assert_eq!(blocks.packet_block_exp(&[0b1111_1110]), Some(11));

        assert_eq!(VorbisBlocks::packet_duration(8, 8), 128);
        assert_eq!(VorbisBlocks::packet_duration(8, 11), 64 + 512);
        assert_eq!(VorbisBlocks::packet_duration(11, 11), 1024);
    }

    #[test]
    fn lookup1_values_is_exact() {
        for dimensions in 1..=6u32 {
            for entries in (0..=70u32).chain([1000, 4096, 4097, 65535, 1 << 24, (1 << 24) - 1]) {
                let v = lookup1_values(entries, dimensions).unwrap();
                assert!(v.pow(dimensions) <= u64::from(entries), "{entries} {dimensions}");
                assert!((v + 1).pow(dimensions) > u64::from(entries), "{entries} {dimensions}");
            }
        }

        assert!(lookup1_values(10, 0).is_err());
    }
}
