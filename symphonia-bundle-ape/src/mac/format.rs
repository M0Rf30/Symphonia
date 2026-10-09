// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

use std::io::{Read, Seek, SeekFrom};

use crate::mac::error::{ApeError, ApeResult};

// ---------------------------------------------------------------------------
// Format flag constants
// ---------------------------------------------------------------------------

pub const APE_FORMAT_FLAG_8_BIT: u16 = 1 << 0; // OBSOLETE
#[allow(dead_code)]
pub const APE_FORMAT_FLAG_CRC: u16 = 1 << 1; // OBSOLETE
pub const APE_FORMAT_FLAG_HAS_PEAK_LEVEL: u16 = 1 << 2; // OBSOLETE
pub const APE_FORMAT_FLAG_24_BIT: u16 = 1 << 3; // OBSOLETE
pub const APE_FORMAT_FLAG_HAS_SEEK_ELEMENTS: u16 = 1 << 4;
pub const APE_FORMAT_FLAG_CREATE_WAV_HEADER: u16 = 1 << 5;

// ---------------------------------------------------------------------------
// Validation constants
// ---------------------------------------------------------------------------

pub const APE_MINIMUM_CHANNELS: u16 = 1;
pub const APE_MAXIMUM_CHANNELS: u16 = 32;
const APE_ONE_MILLION: u32 = 1_000_000;
const APE_WAV_HEADER_OR_FOOTER_MAXIMUM_BYTES: u64 = 8 * 1024 * 1024;
const FIND_DESCRIPTOR_MAX_SCAN: u64 = 1_048_576; // 1 MB

// Magic bytes
const MAGIC_MAC_SPACE: &[u8; 4] = b"MAC ";
const MAGIC_MACF: &[u8; 4] = b"MACF";

// Descriptor / header sizes on disk
const APE_DESCRIPTOR_BYTES: u32 = 52;
const APE_HEADER_BYTES: u32 = 24;

// Old header size on disk
const APE_HEADER_OLD_BYTES: u32 = 32;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// Parsed APE descriptor (52 bytes minimum, all fields little-endian on disk).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ApeDescriptor {
    pub magic: [u8; 4],
    pub version: u16,
    pub padding: u16,
    pub descriptor_bytes: u32,
    pub header_bytes: u32,
    pub seek_table_bytes: u32,
    pub header_data_bytes: u32,
    pub frame_data_bytes: u32,
    pub frame_data_bytes_high: u32,
    pub terminating_data_bytes: u32,
    pub md5: [u8; 16],
}

/// Parsed APE header (24 bytes minimum, all fields little-endian on disk).
#[derive(Debug, Clone)]
pub struct ApeHeader {
    pub compression_level: u16,
    pub format_flags: u16,
    pub blocks_per_frame: u32,
    pub final_frame_blocks: u32,
    pub total_frames: u32,
    pub bits_per_sample: u16,
    pub channels: u16,
    pub sample_rate: u32,
}

/// All parsed and derived information about an APE file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ApeFileInfo {
    // Parsed structures
    pub descriptor: ApeDescriptor,
    pub header: ApeHeader,

    // Seek table (64-bit corrected values, **not** including junk_header_bytes)
    pub seek_table: Vec<u64>,

    // Junk / header data
    pub junk_header_bytes: u32,
    pub wav_header_data: Vec<u8>,

    // Derived values
    pub total_blocks: i64,
    pub block_align: u16,
    pub bytes_per_sample: u16,
    pub wav_data_bytes: i64,
    pub length_ms: i64,
    pub average_bitrate: i64,
    pub decompressed_bitrate: i64,
    pub seek_table_elements: i32,
    pub ape_frame_data_bytes: u64,
    pub terminating_data_bytes: u32,

    // File-level
    pub file_bytes: u64,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn read_u16_le<R: Read>(r: &mut R) -> ApeResult<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32_le<R: Read>(r: &mut R) -> ApeResult<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Convert a 32-bit seek table into 64-bit values, detecting 4 GB wraparound.
fn convert_32bit_seek_table(raw: &[u32]) -> Vec<u64> {
    let mut result = Vec::with_capacity(raw.len());
    let mut add: u64 = 0;
    let mut previous: u32 = 0;
    for &val in raw {
        if val < previous {
            add += 0x1_0000_0000_u64;
        }
        result.push(add + val as u64);
        previous = val;
    }
    result
}

// ---------------------------------------------------------------------------
// FindDescriptor - scan for "MAC " or "MACF" magic, handling ID3v2 junk
// ---------------------------------------------------------------------------

fn find_descriptor<R: Read + Seek>(reader: &mut R) -> ApeResult<u32> {
    reader.seek(SeekFrom::Start(0))?;

    let mut junk_bytes: u32 = 0;

    // Step 1: check for ID3v2 tag
    let mut id3_header = [0u8; 10];
    if reader.read_exact(&mut id3_header).is_ok() && &id3_header[0..3] == b"ID3" {
        let flags = id3_header[5];
        let sync_safe_len: u32 = ((id3_header[6] & 0x7F) as u32) << 21
            | ((id3_header[7] & 0x7F) as u32) << 14
            | ((id3_header[8] & 0x7F) as u32) << 7
            | ((id3_header[9] & 0x7F) as u32);

        let has_footer = flags & (1 << 4) != 0;
        if has_footer {
            junk_bytes = sync_safe_len + 20;
        }
        else {
            junk_bytes = sync_safe_len + 10;

            // Scan past zero-byte padding
            reader.seek(SeekFrom::Start(junk_bytes as u64))?;
            let mut byte = [0u8; 1];
            loop {
                match reader.read_exact(&mut byte) {
                    Ok(()) if byte[0] == 0x00 => junk_bytes += 1,
                    _ => break,
                }
            }
        }
    }

    // Step 2: seek to junk_bytes and read initial 4-byte window
    reader.seek(SeekFrom::Start(junk_bytes as u64))?;
    let mut window = [0u8; 4];
    reader.read_exact(&mut window)?;

    // Check initial window
    if &window == MAGIC_MAC_SPACE || &window == MAGIC_MACF {
        return Ok(junk_bytes);
    }

    // Step 3: scan byte-by-byte up to 1 MB
    let mut scanned: u64 = 4;
    let mut byte = [0u8; 1];
    while scanned < FIND_DESCRIPTOR_MAX_SCAN {
        if reader.read_exact(&mut byte).is_err() {
            break;
        }
        // Shift window left by one byte
        window[0] = window[1];
        window[1] = window[2];
        window[2] = window[3];
        window[3] = byte[0];
        scanned += 1;

        if &window == MAGIC_MAC_SPACE || &window == MAGIC_MACF {
            // The magic starts at (junk_bytes + scanned - 4)
            let offset = junk_bytes as u64 + scanned - 4;
            return Ok(offset as u32);
        }
    }

    Err(ApeError::InvalidFormat("could not find APE descriptor magic"))
}

// ---------------------------------------------------------------------------
// Read descriptor
// ---------------------------------------------------------------------------

fn read_descriptor<R: Read>(reader: &mut R) -> ApeResult<ApeDescriptor> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;

    let version = read_u16_le(reader)?;
    let padding = read_u16_le(reader)?;
    let descriptor_bytes = read_u32_le(reader)?;
    let header_bytes = read_u32_le(reader)?;
    let seek_table_bytes = read_u32_le(reader)?;
    let header_data_bytes = read_u32_le(reader)?;
    let frame_data_bytes = read_u32_le(reader)?;
    let frame_data_bytes_high = read_u32_le(reader)?;
    let terminating_data_bytes = read_u32_le(reader)?;

    let mut md5 = [0u8; 16];
    reader.read_exact(&mut md5)?;

    Ok(ApeDescriptor {
        magic,
        version,
        padding,
        descriptor_bytes,
        header_bytes,
        seek_table_bytes,
        header_data_bytes,
        frame_data_bytes,
        frame_data_bytes_high,
        terminating_data_bytes,
        md5,
    })
}

// ---------------------------------------------------------------------------
// Read header (current format, version >= 3980)
// ---------------------------------------------------------------------------

fn read_header<R: Read>(reader: &mut R) -> ApeResult<ApeHeader> {
    let compression_level = read_u16_le(reader)?;
    let format_flags = read_u16_le(reader)?;
    let blocks_per_frame = read_u32_le(reader)?;
    let final_frame_blocks = read_u32_le(reader)?;
    let total_frames = read_u32_le(reader)?;
    let bits_per_sample = read_u16_le(reader)?;
    let channels = read_u16_le(reader)?;
    let sample_rate = read_u32_le(reader)?;

    Ok(ApeHeader {
        compression_level,
        format_flags,
        blocks_per_frame,
        final_frame_blocks,
        total_frames,
        bits_per_sample,
        channels,
        sample_rate,
    })
}

// ---------------------------------------------------------------------------
// Read old header (version < 3980)
// ---------------------------------------------------------------------------

fn read_old_header<R: Read + Seek>(
    reader: &mut R,
    magic: [u8; 4],
    version: u16,
) -> ApeResult<(ApeDescriptor, ApeHeader, Vec<u8>)> {
    // We've already consumed magic (4) + version (2) = 6 bytes.
    // Old header is 32 bytes total; read remaining 26 bytes worth of fields.
    let compression_level = read_u16_le(reader)?;
    let format_flags = read_u16_le(reader)?;
    let channels = read_u16_le(reader)?;
    let sample_rate = read_u32_le(reader)?;
    let wav_header_bytes = read_u32_le(reader)?;
    let terminating_bytes = read_u32_le(reader)?;
    let total_frames = read_u32_le(reader)?;
    let final_frame_blocks = read_u32_le(reader)?;

    if total_frames == 0 {
        return Err(ApeError::InvalidFormat("old format: total frames is 0 (non-finalized file)"));
    }

    // Derive bits_per_sample from format flags
    let bits_per_sample = if format_flags & APE_FORMAT_FLAG_8_BIT != 0 {
        8u16
    }
    else if format_flags & APE_FORMAT_FLAG_24_BIT != 0 {
        24u16
    }
    else {
        16u16
    };

    // Derive blocks_per_frame from version and compression level
    let blocks_per_frame: u32 = if version >= 3950 {
        73728 * 4
    }
    else if version >= 3900 || (version >= 3800 && compression_level == 4000) {
        73728
    }
    else {
        9216
    };

    // Read optional fields after header
    let mut _peak_level: u32 = 0;
    if format_flags & APE_FORMAT_FLAG_HAS_PEAK_LEVEL != 0 {
        _peak_level = read_u32_le(reader)?;
    }

    let seek_table_elements: u32 = if format_flags & APE_FORMAT_FLAG_HAS_SEEK_ELEMENTS != 0 {
        read_u32_le(reader)?
    }
    else {
        total_frames
    };

    // Cap at 1M entries (~4MB) to prevent OOM from malformed headers
    if seek_table_elements > 1_000_000 {
        return Err(ApeError::InvalidFormat("seek table too large"));
    }

    // Read WAV header data
    let mut wav_header_data = Vec::new();
    if format_flags & APE_FORMAT_FLAG_CREATE_WAV_HEADER == 0 && wav_header_bytes > 0 {
        if (wav_header_bytes as u64) > APE_WAV_HEADER_OR_FOOTER_MAXIMUM_BYTES {
            return Err(ApeError::InvalidFormat("WAV header data exceeds 8 MB limit"));
        }
        wav_header_data.resize(wav_header_bytes as usize, 0);
        reader.read_exact(&mut wav_header_data)?;
    }

    // Read seek table (u32 entries)
    let seek_table_bytes = seek_table_elements * 4;
    let mut seek_raw = vec![0u32; seek_table_elements as usize];
    for entry in seek_raw.iter_mut() {
        *entry = read_u32_le(reader)?;
    }

    // Skip seek bit table for version <= 3800
    if version <= 3800 {
        // seek bit table: 1 byte per element
        reader.seek(SeekFrom::Current(seek_table_elements as i64))?;
    }

    // Build a synthetic descriptor
    let descriptor = ApeDescriptor {
        magic,
        version,
        padding: 0,
        descriptor_bytes: 0, // no descriptor in old format
        header_bytes: APE_HEADER_OLD_BYTES,
        seek_table_bytes,
        header_data_bytes: wav_header_bytes,
        frame_data_bytes: 0,
        frame_data_bytes_high: 0,
        terminating_data_bytes: terminating_bytes,
        md5: [0u8; 16],
    };

    let header = ApeHeader {
        compression_level,
        format_flags,
        blocks_per_frame,
        final_frame_blocks,
        total_frames,
        bits_per_sample,
        channels,
        sample_rate,
    };

    Ok((descriptor, header, wav_header_data))
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate(descriptor: &ApeDescriptor, header: &ApeHeader, file_bytes: u64) -> ApeResult<()> {
    // Channel count
    if header.channels < APE_MINIMUM_CHANNELS || header.channels > APE_MAXIMUM_CHANNELS {
        return Err(ApeError::InvalidFormat("channel count out of range (must be 1..=32)"));
    }

    // Blocks per frame
    if header.blocks_per_frame == 0 {
        return Err(ApeError::InvalidFormat("blocks per frame is 0"));
    }

    if header.compression_level >= 5000 {
        if header.blocks_per_frame > 10 * APE_ONE_MILLION {
            return Err(ApeError::InvalidFormat(
                "blocks per frame exceeds 10,000,000 for insane compression",
            ));
        }
    }
    else if header.blocks_per_frame > APE_ONE_MILLION {
        return Err(ApeError::InvalidFormat("blocks per frame exceeds 1,000,000"));
    }

    // Final frame blocks
    if header.final_frame_blocks > header.blocks_per_frame {
        return Err(ApeError::InvalidFormat("final frame blocks exceeds blocks per frame"));
    }

    // Seek table elements sanity
    let seek_table_elements = descriptor.seek_table_bytes / 4;
    if file_bytes > 0 && (seek_table_elements as u64) > file_bytes / 4 {
        return Err(ApeError::InvalidFormat("seek table elements exceed file size / 4"));
    }

    // WAV header data size
    if (descriptor.header_data_bytes as u64) > APE_WAV_HEADER_OR_FOOTER_MAXIMUM_BYTES {
        return Err(ApeError::InvalidFormat("WAV header data exceeds 8 MB limit"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Main parse function
// ---------------------------------------------------------------------------

/// Parse an APE file, returning all metadata and derived values.
///
/// Supports both current format (version >= 3980) and old format (version < 3980).
pub fn parse<R: Read + Seek>(reader: &mut R) -> ApeResult<ApeFileInfo> {
    // Get file size
    let file_bytes = reader.seek(SeekFrom::End(0))?;

    // Find descriptor magic
    let junk_header_bytes = find_descriptor(reader)?;

    // Seek to descriptor start
    reader.seek(SeekFrom::Start(junk_header_bytes as u64))?;

    // Peek at magic + version to decide format
    let mut peek_buf = [0u8; 6];
    reader.read_exact(&mut peek_buf)?;
    let magic: [u8; 4] = [peek_buf[0], peek_buf[1], peek_buf[2], peek_buf[3]];
    let version = u16::from_le_bytes([peek_buf[4], peek_buf[5]]);

    if version < 3980 {
        // Old format
        let (descriptor, header, wav_header_data) = read_old_header(reader, magic, version)?;

        // Compute derived values
        let total_blocks: i64 = if header.total_frames == 0 {
            0
        }
        else {
            (header.total_frames as i64 - 1) * header.blocks_per_frame as i64
                + header.final_frame_blocks as i64
        };

        let bytes_per_sample = header.bits_per_sample / 8;
        let block_align = (bytes_per_sample as u32 * header.channels as u32) as u16;
        let wav_data_bytes = total_blocks.saturating_mul(block_align as i64);
        let length_ms = if header.sample_rate > 0 {
            total_blocks.saturating_mul(1000) / header.sample_rate as i64
        }
        else {
            0
        };

        let ape_frame_data_bytes: u64 =
            (descriptor.frame_data_bytes_high as u64) << 32 | descriptor.frame_data_bytes as u64;

        let ape_total_bytes = file_bytes as i64;
        let average_bitrate =
            if length_ms > 0 { ape_total_bytes.saturating_mul(8) / length_ms } else { 0 };
        let decompressed_bitrate = if header.sample_rate > 0 {
            (block_align as i64).saturating_mul(header.sample_rate as i64).saturating_mul(8) / 1000
        }
        else {
            0
        };

        let seek_table_elements = (descriptor.seek_table_bytes / 4) as i32;

        // The seek table was already read by read_old_header; we need to reconstruct it.
        // Re-read it: seek back to the right position.
        // Actually, we need a different approach - let's re-parse more carefully.
        // For old format, the seek table was already read in read_old_header.
        // Let's refactor to pass it through.

        // For now, return empty seek table and note this limitation.
        // Actually, let me refactor read_old_header to also return the seek table.
        // ... We'll reconstruct from the raw read above.

        // Since read_old_header consumed the seek table, we need to re-approach.
        // Let me re-read from the file.
        // Actually, let's just re-do this properly.

        // We already read past everything in read_old_header. Let's re-seek and re-read.
        // The seek table starts after: junk + 32 (old header) + optional peak (4) + optional seek_elements (4) + wav_header
        let mut seek_offset = junk_header_bytes as u64 + APE_HEADER_OLD_BYTES as u64;
        if header.format_flags & APE_FORMAT_FLAG_HAS_PEAK_LEVEL != 0 {
            seek_offset += 4;
        }
        if header.format_flags & APE_FORMAT_FLAG_HAS_SEEK_ELEMENTS != 0 {
            seek_offset += 4;
        }
        if header.format_flags & APE_FORMAT_FLAG_CREATE_WAV_HEADER == 0 {
            seek_offset += descriptor.header_data_bytes as u64;
        }

        reader.seek(SeekFrom::Start(seek_offset))?;
        let n_seek = seek_table_elements as usize;
        let mut seek_raw = vec![0u32; n_seek];
        for entry in seek_raw.iter_mut() {
            *entry = read_u32_le(reader)?;
        }
        let seek_table = convert_32bit_seek_table(&seek_raw);

        validate(&descriptor, &header, file_bytes)?;

        return Ok(ApeFileInfo {
            descriptor,
            header,
            seek_table,
            junk_header_bytes,
            wav_header_data,
            total_blocks,
            block_align,
            bytes_per_sample,
            wav_data_bytes,
            length_ms,
            average_bitrate,
            decompressed_bitrate,
            seek_table_elements,
            ape_frame_data_bytes,
            terminating_data_bytes: 0,
            file_bytes,
        });
    }

    // Current format (version >= 3980)
    // Seek back to descriptor start (we already consumed 6 bytes for the peek)
    reader.seek(SeekFrom::Start(junk_header_bytes as u64))?;

    // Read descriptor (52 bytes)
    let descriptor = read_descriptor(reader)?;

    // Skip extra descriptor bytes
    if descriptor.descriptor_bytes > APE_DESCRIPTOR_BYTES {
        reader
            .seek(SeekFrom::Current((descriptor.descriptor_bytes - APE_DESCRIPTOR_BYTES) as i64))?;
    }

    // Read header (24 bytes)
    let header = read_header(reader)?;

    // Skip extra header bytes
    if descriptor.header_bytes > APE_HEADER_BYTES {
        reader.seek(SeekFrom::Current((descriptor.header_bytes - APE_HEADER_BYTES) as i64))?;
    }

    // Read seek table (u32 entries, then convert to u64)
    let seek_table_elements = (descriptor.seek_table_bytes / 4) as i32;
    if seek_table_elements < 0 || seek_table_elements > 1_000_000 {
        return Err(ApeError::InvalidFormat("seek table too large"));
    }
    if file_bytes > 0 && (seek_table_elements as u64) > file_bytes / 4 {
        return Err(ApeError::InvalidFormat("seek table elements exceed file size"));
    }
    let mut seek_raw = vec![0u32; seek_table_elements as usize];
    for entry in seek_raw.iter_mut() {
        *entry = read_u32_le(reader)?;
    }
    let seek_table = convert_32bit_seek_table(&seek_raw);

    // Read WAV header data
    let mut wav_header_data = Vec::new();
    if descriptor.header_data_bytes > 0 {
        if (descriptor.header_data_bytes as u64) > APE_WAV_HEADER_OR_FOOTER_MAXIMUM_BYTES {
            return Err(ApeError::InvalidFormat("WAV header data exceeds 8 MB limit"));
        }
        wav_header_data.resize(descriptor.header_data_bytes as usize, 0);
        reader.read_exact(&mut wav_header_data)?;
    }

    // Validate
    validate(&descriptor, &header, file_bytes)?;

    // Compute derived values
    let total_blocks: i64 = if header.total_frames == 0 {
        0
    }
    else {
        (header.total_frames as i64 - 1) * header.blocks_per_frame as i64
            + header.final_frame_blocks as i64
    };

    let bytes_per_sample = header.bits_per_sample / 8;
    let block_align = (bytes_per_sample as u32 * header.channels as u32) as u16;
    let wav_data_bytes = total_blocks.saturating_mul(block_align as i64);

    let length_ms = if header.sample_rate > 0 {
        total_blocks.saturating_mul(1000) / header.sample_rate as i64
    }
    else {
        0
    };

    let ape_frame_data_bytes: u64 =
        (descriptor.frame_data_bytes_high as u64) << 32 | descriptor.frame_data_bytes as u64;

    let ape_total_bytes = file_bytes as i64;
    let average_bitrate =
        if length_ms > 0 { ape_total_bytes.saturating_mul(8) / length_ms } else { 0 };

    let decompressed_bitrate = if header.sample_rate > 0 {
        (block_align as i64).saturating_mul(header.sample_rate as i64).saturating_mul(8) / 1000
    }
    else {
        0
    };

    Ok(ApeFileInfo {
        descriptor,
        header,
        seek_table,
        junk_header_bytes,
        wav_header_data,
        total_blocks,
        block_align,
        bytes_per_sample,
        wav_data_bytes,
        length_ms,
        average_bitrate,
        decompressed_bitrate,
        seek_table_elements,
        ape_frame_data_bytes,
        terminating_data_bytes: 0,
        file_bytes,
    })
}

// ---------------------------------------------------------------------------
// Helper methods on ApeFileInfo
// ---------------------------------------------------------------------------

impl ApeFileInfo {
    /// Returns the number of audio blocks in the given frame.
    ///
    /// All frames except the last have `blocks_per_frame` blocks; the last
    /// frame has `final_frame_blocks`.
    pub fn frame_block_count(&self, frame_idx: u32) -> u32 {
        if self.header.total_frames == 0 {
            return 0;
        }
        if frame_idx == self.header.total_frames - 1 {
            self.header.final_frame_blocks
        }
        else {
            self.header.blocks_per_frame
        }
    }

    /// Returns the compressed byte count for the given frame.
    ///
    /// For non-final frames this is the difference between consecutive seek
    /// table entries. For the final frame it is the distance from its seek
    /// position to the end of the compressed data region.
    pub fn frame_byte_count(&self, frame_idx: u32) -> u64 {
        if self.header.total_frames == 0 {
            return 0;
        }
        if frame_idx < self.header.total_frames - 1 {
            self.seek_byte(frame_idx + 1) - self.seek_byte(frame_idx)
        }
        else {
            // Final frame: from seek position to end of compressed data
            // End of compressed data = file_size - terminating_data - tag_bytes
            // For simplicity we exclude terminating data; tag detection would
            // require more work. This matches the SDK pattern.
            let end = self.file_bytes.saturating_sub(self.descriptor.terminating_data_bytes as u64);
            let start = self.seek_byte(frame_idx);
            if end > start { end - start } else { 0 }
        }
    }

    /// Returns the absolute byte offset in the file for the given frame,
    /// including the junk header offset.
    pub fn seek_byte(&self, frame_idx: u32) -> u64 {
        let idx = frame_idx as usize;
        if idx < self.seek_table.len() {
            self.seek_table[idx] + self.junk_header_bytes as u64
        }
        else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
