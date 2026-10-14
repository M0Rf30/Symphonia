// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

use std::io::{Read, Seek, SeekFrom};

use crate::mac::error::{ApeError, ApeResult};

// ---------------------------------------------------------------------------
// Format flag constants
// ---------------------------------------------------------------------------

pub const APE_FORMAT_FLAG_8_BIT: u16 = 1 << 0; // OBSOLETE
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

    /// The format flags of the header (`APE_FORMAT_FLAG_*`). Only those of the files older than
    /// version 3980 are of use to the decoder.
    pub format_flags: u16,

    /// For the files up to version 3800, whose frames do not start on a byte boundary: the bit of
    /// the byte of the seek table each frame starts at.
    pub seek_bit_table: Vec<u8>,

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

    /// The offset of the end of the compressed data of the last frame: the end of the frame data
    /// that the descriptor declares if the file holds it, and otherwise the end of the file, less
    /// the terminating data and the tags (APEv2, ID3v1) at its end (as `CAPEInfo::GetInfo` of the
    /// SDK).
    pub data_end: u64,

    /// The offset of the end of the compressed data of the last frame that the descriptor
    /// declares: the end of the frame data, without the terminating data and the tags. Zero if the
    /// header does not declare it (the files older than version 3980, and files that have no
    /// frame data size in their descriptor). A file that is shorter than this is truncated.
    pub declared_data_end: u64,
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

fn read_header<R: Read>(reader: &mut R) -> ApeResult<(ApeHeader, u16)> {
    let compression_level = read_u16_le(reader)?;
    let format_flags = read_u16_le(reader)?;
    let blocks_per_frame = read_u32_le(reader)?;
    let final_frame_blocks = read_u32_le(reader)?;
    let total_frames = read_u32_le(reader)?;
    let bits_per_sample = read_u16_le(reader)?;
    let channels = read_u16_le(reader)?;
    let sample_rate = read_u32_le(reader)?;

    Ok((
        ApeHeader {
            compression_level,
            blocks_per_frame,
            final_frame_blocks,
            total_frames,
            bits_per_sample,
            channels,
            sample_rate,
        },
        format_flags,
    ))
}

// ---------------------------------------------------------------------------
// Read old header (version < 3980)
// ---------------------------------------------------------------------------

/// The parts of the header of a file older than version 3980.
struct OldHeader {
    descriptor: ApeDescriptor,
    header: ApeHeader,
    format_flags: u16,
    wav_header_data: Vec<u8>,
    seek_table: Vec<u64>,
    seek_bit_table: Vec<u8>,
}

fn read_old_header<R: Read + Seek>(
    reader: &mut R,
    magic: [u8; 4],
    version: u16,
) -> ApeResult<OldHeader> {
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

    // The seek bit table of the version <= 3800: 1 byte per element
    let mut seek_bit_table = Vec::new();
    if version <= 3800 {
        seek_bit_table.resize(seek_table_elements as usize, 0);
        reader.read_exact(&mut seek_bit_table)?;
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
        blocks_per_frame,
        final_frame_blocks,
        total_frames,
        bits_per_sample,
        channels,
        sample_rate,
    };

    Ok(OldHeader {
        descriptor,
        header,
        format_flags,
        wav_header_data,
        seek_table: convert_32bit_seek_table(&seek_raw),
        seek_bit_table,
    })
}

// ---------------------------------------------------------------------------
// Tags at the end of the file
// ---------------------------------------------------------------------------

const ID3V1_TAG_BYTES: u64 = 128;
const APE_TAG_FOOTER_BYTES: u64 = 32;
const APE_TAG_FLAG_CONTAINS_HEADER: u32 = 1 << 31;
const APE_TAG_FLAG_IS_HEADER: u32 = 1 << 29;
const APE_TAG_MAXIMUM_FIELD_BYTES: i64 = 256 * 1024 * 1024;

/// The size of the tags at the end of the file: an ID3v1 tag, and an APEv2 tag before it, as
/// detected by `CAPETag::Analyze` of the SDK.
fn trailing_tag_bytes<R: Read + Seek>(reader: &mut R, file_bytes: u64) -> ApeResult<u64> {
    let mut tag_bytes = 0;

    if file_bytes > ID3V1_TAG_BYTES {
        reader.seek(SeekFrom::Start(file_bytes - ID3V1_TAG_BYTES))?;
        let mut id3 = [0u8; 3];
        reader.read_exact(&mut id3)?;
        if &id3 == b"TAG" {
            tag_bytes = ID3V1_TAG_BYTES;
        }
    }

    if file_bytes >= tag_bytes + APE_TAG_FOOTER_BYTES {
        reader.seek(SeekFrom::Start(file_bytes - tag_bytes - APE_TAG_FOOTER_BYTES))?;
        let mut footer = [0u8; APE_TAG_FOOTER_BYTES as usize];
        reader.read_exact(&mut footer)?;

        let le_i32 = |at: usize| {
            i32::from_le_bytes([footer[at], footer[at + 1], footer[at + 2], footer[at + 3]])
        };
        let (version, size, fields, flags) = (le_i32(8), le_i32(12), le_i32(16), le_i32(20) as u32);

        let valid = &footer[..8] == b"APETAGEX"
            && version <= 2000
            && fields <= 65536
            && i64::from(size) >= APE_TAG_FOOTER_BYTES as i64
            && i64::from(size) - APE_TAG_FOOTER_BYTES as i64 <= APE_TAG_MAXIMUM_FIELD_BYTES
            && flags & APE_TAG_FLAG_IS_HEADER == 0;

        if valid {
            let header =
                if flags & APE_TAG_FLAG_CONTAINS_HEADER != 0 { APE_TAG_FOOTER_BYTES } else { 0 };
            tag_bytes += size as u64 + header;
        }
    }

    Ok(tag_bytes.min(file_bytes))
}

/// The offset of the end of the compressed data (see `ApeFileInfo::data_end`).
fn data_end<R: Read + Seek>(
    reader: &mut R,
    file_bytes: u64,
    terminating_data_bytes: u32,
) -> ApeResult<u64> {
    let tag_bytes = trailing_tag_bytes(reader, file_bytes)?;
    Ok(file_bytes.saturating_sub(tag_bytes).saturating_sub(u64::from(terminating_data_bytes)))
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate(
    descriptor: &ApeDescriptor,
    header: &ApeHeader,
    seek_table_len: usize,
    file_bytes: u64,
) -> ApeResult<()> {
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

    // Every frame needs a seek table entry.
    if (seek_table_len as u64) < u64::from(header.total_frames) {
        return Err(ApeError::InvalidFormat("seek table has fewer entries than frames"));
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
        let OldHeader {
            descriptor,
            header,
            format_flags,
            wav_header_data,
            seek_table,
            seek_bit_table,
        } = read_old_header(reader, magic, version)?;

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

        let seek_table_elements = seek_table.len() as i32;

        validate(&descriptor, &header, seek_table.len(), file_bytes)?;

        let data_end = data_end(reader, file_bytes, descriptor.terminating_data_bytes)?;

        return Ok(ApeFileInfo {
            descriptor,
            header,
            seek_table,
            format_flags,
            seek_bit_table,
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
            data_end,
            declared_data_end: 0,
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
    let (header, format_flags) = read_header(reader)?;

    // Skip extra header bytes
    if descriptor.header_bytes > APE_HEADER_BYTES {
        reader.seek(SeekFrom::Current((descriptor.header_bytes - APE_HEADER_BYTES) as i64))?;
    }

    // Read seek table (u32 entries, then convert to u64)
    let seek_table_elements = (descriptor.seek_table_bytes / 4) as i32;
    if !(0..=1_000_000).contains(&seek_table_elements) {
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
    validate(&descriptor, &header, seek_table.len(), file_bytes)?;

    let tags_start = data_end(reader, file_bytes, descriptor.terminating_data_bytes)?;

    let frame_data_bytes =
        u64::from(descriptor.frame_data_bytes_high) << 32 | u64::from(descriptor.frame_data_bytes);
    let declared_data_end = if frame_data_bytes == 0 {
        0
    }
    else {
        u64::from(junk_header_bytes)
            + u64::from(descriptor.descriptor_bytes)
            + u64::from(descriptor.header_bytes)
            + u64::from(descriptor.seek_table_bytes)
            + u64::from(descriptor.header_data_bytes)
            + frame_data_bytes
    };

    // The end of the frame data that the descriptor declares is a better end of the last frame than
    // the file less its terminating data and tags, if it is possible: a corrupt tag makes the size
    // of the tags unreliable.
    let last_frame_start = seek_table
        .get(header.total_frames.saturating_sub(1) as usize)
        .map_or(0, |&b| b + u64::from(junk_header_bytes));
    let data_end = if declared_data_end > last_frame_start && declared_data_end <= file_bytes {
        declared_data_end
    }
    else {
        tags_start
    };

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
        format_flags,
        seek_bit_table: Vec::new(),
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
        data_end,
        declared_data_end,
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
            self.seek_byte(frame_idx + 1).saturating_sub(self.seek_byte(frame_idx))
        }
        else {
            // Final frame: from seek position to end of compressed data
            // End of compressed data = file_size - terminating_data - tag_bytes
            // (`CAPEInfo::GetInfo(APE_INFO_FRAME_BYTES)` of the SDK).
            self.data_end.saturating_sub(self.seek_byte(frame_idx))
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn new_format_file(total_frames: u32, seek_entries: &[u32]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"MAC ");
        buf.extend_from_slice(&3990u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&APE_DESCRIPTOR_BYTES.to_le_bytes());
        buf.extend_from_slice(&APE_HEADER_BYTES.to_le_bytes());
        buf.extend_from_slice(&((seek_entries.len() * 4) as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // header data bytes
        buf.extend_from_slice(&0u32.to_le_bytes()); // frame data bytes
        buf.extend_from_slice(&0u32.to_le_bytes()); // frame data bytes (high)
        buf.extend_from_slice(&0u32.to_le_bytes()); // terminating data bytes
        buf.extend_from_slice(&[0u8; 16]); // md5
        buf.extend_from_slice(&2000u16.to_le_bytes()); // compression level
        buf.extend_from_slice(&0u16.to_le_bytes()); // format flags
        buf.extend_from_slice(&73728u32.to_le_bytes()); // blocks per frame
        buf.extend_from_slice(&100u32.to_le_bytes()); // final frame blocks
        buf.extend_from_slice(&total_frames.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        buf.extend_from_slice(&2u16.to_le_bytes()); // channels
        buf.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        for e in seek_entries {
            buf.extend_from_slice(&e.to_le_bytes());
        }
        buf.extend_from_slice(&[0u8; 64]);
        buf
    }

    #[test]
    fn verify_seek_table_shorter_than_total_frames_is_rejected() {
        let buf = new_format_file(3, &[100]);
        let res = parse(&mut Cursor::new(buf));
        assert!(matches!(res, Err(ApeError::InvalidFormat(_))));
    }

    #[test]
    fn verify_seek_table_matching_total_frames_is_accepted() {
        let buf = new_format_file(2, &[100, 120]);
        assert!(parse(&mut Cursor::new(buf)).is_ok());
    }
}
