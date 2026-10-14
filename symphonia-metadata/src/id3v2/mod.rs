// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ID3v2 metadata reader.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::ops::Range;

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::formats::probe::{
    Anchors, ProbeMetadataData, ProbeableMetadata, Score, Scoreable,
};
use symphonia_core::io::*;
use symphonia_core::meta::well_known::METADATA_ID_ID3V2;
use symphonia_core::meta::{
    ChapterGroup, ChapterGroupItem, MetadataBuffer, MetadataBuilder, MetadataInfo, MetadataOptions,
    MetadataReader, MetadataSideData,
};
use symphonia_core::support_metadata;

use log::{debug, trace, warn};

mod frames;
mod legacy_date;
mod unsync;
#[cfg(feature = "write")]
pub mod write;

#[cfg(test)]
mod content_tests;

use crate::utils::ape::{ApeTagFlags, HEADER_LEN as APE_HEADER_LEN};
use frames::*;
use unsync::{decode_synchsafe, decode_unsynchronisation, read_syncsafe_leq32};

#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
enum TagSizeRestriction {
    Max128Frames1024KiB,
    Max64Frames128KiB,
    Max32Frames40KiB,
    Max32Frames4KiB,
}

#[derive(Debug)]
enum TextEncodingRestriction {
    None,
    Utf8OrIso88591,
}

#[derive(Debug)]
enum TextFieldSize {
    None,
    Max1024Characters,
    Max128Characters,
    Max30Characters,
}

#[derive(Debug)]
enum ImageEncodingRestriction {
    None,
    PngOrJpegOnly,
}

#[derive(Debug)]
enum ImageSizeRestriction {
    None,
    LessThan256x256,
    LessThan64x64,
    Exactly64x64,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Header {
    major_version: u8,
    minor_version: u8,
    size: u32,
    unsynchronisation: bool,
    has_extended_header: bool,
    experimental: bool,
    has_footer: bool,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Restrictions {
    tag_size: TagSizeRestriction,
    text_encoding: TextEncodingRestriction,
    text_field_size: TextFieldSize,
    image_encoding: ImageEncodingRestriction,
    image_size: ImageSizeRestriction,
}

#[derive(Debug)]
#[allow(dead_code)]
struct ExtendedHeader {
    /// ID3v2.3 only, the number of padding bytes.
    padding_size: Option<u32>,
    /// ID3v2.3+, a CRC32 checksum of the Tag.
    crc32: Option<u32>,
    /// ID3v2.4 only, is this Tag an update to an earlier Tag.
    is_update: Option<bool>,
    /// ID3v2.4 only, Tag modification restrictions.
    restrictions: Option<Restrictions>,
}

/// The fields of a structurally valid 10 byte ID3v2 header.
struct HeaderBytes {
    major: u8,
    flags: u8,
    /// The size of the tag, excluding the header and footer.
    size: u32,
}

impl HeaderBytes {
    /// ID3v2.2 compression is undefined, so a tag that uses it cannot be read.
    fn is_supported(&self) -> bool {
        !(self.major == 2 && self.flags & 0x40 != 0)
    }
}

/// Parse a 10 byte ID3v2 header. Returns `None` if `hdr` does not start with the "ID3" marker, a
/// supported version, and a syncsafe size.
fn parse_header_bytes(hdr: &[u8]) -> Option<HeaderBytes> {
    let [b'I', b'D', b'3', major, minor, flags, size @ ..] = hdr.get(..10)?
    else {
        return None;
    };

    if !(2..=4).contains(major) || *minor == 0xff || size.iter().any(|b| b & 0x80 != 0) {
        return None;
    }

    Some(HeaderBytes { major: *major, flags: *flags, size: decode_synchsafe(size) })
}

/// Read the header of an ID3v2 (verions 2.2+) tag.
fn read_id3v2_header<B: ReadBytes>(reader: &mut B) -> Result<Header> {
    let marker = reader.read_triple_bytes()?;

    if marker != *b"ID3" {
        return unsupported_error("id3v2: not an ID3v2 tag");
    }

    let major_version = reader.read_u8()?;
    let minor_version = reader.read_u8()?;
    let flags = reader.read_u8()?;
    let size = unsync::read_syncsafe_leq32(reader, 28)?;

    let mut header = Header {
        major_version,
        minor_version,
        size,
        unsynchronisation: false,
        has_extended_header: false,
        experimental: false,
        has_footer: false,
    };

    // Major and minor version numbers should never equal 0xff as per the specification.
    if major_version == 0xff || minor_version == 0xff {
        return decode_error("id3v2: invalid version number(s)");
    }

    // Only support versions 2.2.x (first version) to 2.4.x (latest version as of May 2019) of the
    // specification.
    if major_version < 2 || major_version > 4 {
        return unsupported_error("id3v2: unsupported ID3v2 version");
    }

    // Version 2.2 of the standard specifies a compression flag bit, but does not specify a
    // compression standard. Future versions of the standard remove this feature and repurpose this
    // bit for other features. Since there is no way to know how to handle the remaining tag data,
    // return an unsupported error.
    if major_version == 2 && (flags & 0x40) != 0 {
        return unsupported_error("id3v2: ID3v2.2 compression is not supported");
    }

    // With the exception of the compression flag in version 2.2, flags were added sequentially each
    // major version. Check each bit sequentially as they appear in each version.
    if major_version >= 2 {
        header.unsynchronisation = flags & 0x80 != 0;
    }

    if major_version >= 3 {
        header.has_extended_header = flags & 0x40 != 0;
        header.experimental = flags & 0x20 != 0;
    }

    if major_version >= 4 {
        header.has_footer = flags & 0x10 != 0;
    }

    Ok(header)
}

/// Read the extended header of an ID3v2.3 tag.
fn read_id3v2p3_extended_header(reader: &mut BufReader<'_>) -> Result<ExtendedHeader> {
    let size = reader.read_be_u32()?;
    let flags = reader.read_be_u16()?;
    let padding_size = reader.read_be_u32()?;

    if !(size == 6 || size == 10) {
        return decode_error("id3v2: invalid extended header size");
    }

    let mut header = ExtendedHeader {
        padding_size: Some(padding_size),
        crc32: None,
        is_update: None,
        restrictions: None,
    };

    // An extended header with a size of 10 always has space for a CRC32, even if the CRC32 flag is
    // clear. Skip it either way so the frames that follow are found.
    if size == 10 {
        let crc32 = reader.read_be_u32()?;

        // CRC32 flag.
        if flags & 0x8000 != 0 {
            header.crc32 = Some(crc32);
        }
    }

    Ok(header)
}

/// Read the extended header of an ID3v2.4 tag.
fn read_id3v2p4_extended_header(reader: &mut BufReader<'_>) -> Result<ExtendedHeader> {
    // The size of the extended header, including the size field itself.
    let size = read_syncsafe_leq32(reader, 28)?;

    // The smallest extended header is the size field, the number of flag bytes, and one flag byte.
    let Some(len) = size.checked_sub(4).filter(|&len| len >= 2)
    else {
        return decode_error("id3v2: invalid extended header size");
    };

    // Read the remainder of the extended header at once. A malformed or unrecognized field can then
    // not cause the frames that follow the extended header to be missed.
    let data = reader.read_buf_bytes_ref(len as usize)?;

    match parse_id3v2p4_extended_flags(&mut BufReader::new(data)) {
        Ok(header) => Ok(header),
        Err(err) => {
            warn!("id3v2: ignoring a malformed extended header: {err}");

            Ok(ExtendedHeader {
                padding_size: None,
                crc32: None,
                is_update: Some(false),
                restrictions: None,
            })
        }
    }
}

/// Parse the flags, and flag data, of an ID3v2.4 extended header.
fn parse_id3v2p4_extended_flags(reader: &mut BufReader<'_>) -> Result<ExtendedHeader> {
    // The number of flag bytes. Only the first flag byte is defined.
    let num_flag_bytes = reader.read_u8()?;

    if num_flag_bytes == 0 {
        return decode_error("id3v2: extended flags should have a length of at least 1");
    }

    let flags = reader.read_u8()?;

    reader.ignore_bytes(u64::from(num_flag_bytes - 1))?;

    let mut header = ExtendedHeader {
        padding_size: None,
        crc32: None,
        is_update: Some(false),
        restrictions: None,
    };

    // Tag is an update flag. The flag has no data, so the length is 0. Some writers give the flag
    // a length of 1, and a data byte.
    if flags & 0x40 != 0x0 {
        match reader.read_u8()? {
            0 => (),
            1 => {
                reader.read_u8()?;
            }
            _ => return decode_error("id3v2: is update extended flag has invalid size"),
        }

        header.is_update = Some(true);
    }

    // CRC32 flag.
    if flags & 0x20 != 0x0 {
        let len = reader.read_u8()?;
        if len != 5 {
            return decode_error("id3v2: CRC32 extended flag has invalid size");
        }

        header.crc32 = Some(read_syncsafe_leq32(reader, 32)?);
    }

    // Restrictions flag.
    if flags & 0x10 != 0x0 {
        let len = reader.read_u8()?;
        if len != 1 {
            return decode_error("id3v2: restrictions extended flag has invalid size");
        }

        let restrictions = reader.read_u8()?;

        let tag_size = match (restrictions & 0xc0) >> 6 {
            0 => TagSizeRestriction::Max128Frames1024KiB,
            1 => TagSizeRestriction::Max64Frames128KiB,
            2 => TagSizeRestriction::Max32Frames40KiB,
            3 => TagSizeRestriction::Max32Frames4KiB,
            _ => unreachable!(),
        };

        let text_encoding = match (restrictions & 0x20) >> 5 {
            0 => TextEncodingRestriction::None,
            1 => TextEncodingRestriction::Utf8OrIso88591,
            _ => unreachable!(),
        };

        let text_field_size = match (restrictions & 0x18) >> 3 {
            0 => TextFieldSize::None,
            1 => TextFieldSize::Max1024Characters,
            2 => TextFieldSize::Max128Characters,
            3 => TextFieldSize::Max30Characters,
            _ => unreachable!(),
        };

        let image_encoding = match (restrictions & 0x04) >> 2 {
            0 => ImageEncodingRestriction::None,
            1 => ImageEncodingRestriction::PngOrJpegOnly,
            _ => unreachable!(),
        };

        let image_size = match restrictions & 0x03 {
            0 => ImageSizeRestriction::None,
            1 => ImageSizeRestriction::LessThan256x256,
            2 => ImageSizeRestriction::LessThan64x64,
            3 => ImageSizeRestriction::Exactly64x64,
            _ => unreachable!(),
        };

        header.restrictions = Some(Restrictions {
            tag_size,
            text_encoding,
            text_field_size,
            image_encoding,
            image_size,
        })
    }

    Ok(header)
}

fn read_id3v2_body(
    data: &[u8],
    header: &Header,
    metadata: &mut MetadataBuilder,
    side_data: &mut Vec<MetadataSideData>,
) -> Result<()> {
    let mut reader = BufReader::new(data);

    // If there is an extended header, read and parse it based on the major version of the tag.
    if header.has_extended_header {
        let extended = match header.major_version {
            3 => read_id3v2p3_extended_header(&mut reader)?,
            4 => read_id3v2p4_extended_header(&mut reader)?,
            _ => unreachable!(),
        };
        trace!("{:#?}", extended);
    }

    let min_frame_size = min_frame_size(header.major_version);

    let mut chap_builder: ChapterGroupBuilder = Default::default();
    let mut legacy_dates = legacy_date::LegacyDates::default();

    // Read frames until there is not enough bytes available in the ID3v2 tag for another frame.
    while reader.bytes_available() >= min_frame_size {
        // Read frames based on the major version of the tag.
        let frame = match header.major_version {
            2 => read_id3v2p2_frame(&mut reader),
            3 => read_id3v2p3_frame(&mut reader),
            4 => read_id3v2p4_frame(&mut reader),
            _ => break,
        };

        match frame {
            // The frame was skipped for some reason.
            Ok(FrameResult::Skipped) => (),
            // The padding has been reached, don't parse any further.
            Ok(FrameResult::Padding) => break,
            // A frame was parsed into a tag, add it to the tag collection.
            Ok(FrameResult::Tag(tag)) => {
                legacy_dates.observe(&tag);
                metadata.add_tag(tag);
            }
            // A frame was parsed into multiple tags, add them all to the tag collection.
            Ok(FrameResult::MultipleTags(tags)) => {
                for tag in tags {
                    legacy_dates.observe(&tag);
                    metadata.add_tag(tag);
                }
            }
            // A frame was parsed into a visual, add it to the visual collection.
            Ok(FrameResult::Visual(visual)) => {
                metadata.add_visual(visual);
            }
            // A chapter was encountered, save it for post-processing.
            Ok(FrameResult::Chapter(chap)) => {
                chap_builder.add_chapter(chap);
            }
            // A table of contents was encountered, save it for post-processing.
            Ok(FrameResult::TableOfContents(toc)) => {
                chap_builder.add_toc(toc);
            }
            Err(_) => {
                // The read frame functions suppress any errors that occur while reading the content
                // of a frame. Any errors that are returned are related to the frame's structure and
                // result in a fatal error since the structure of the overall tag becomes
                // compromised.
                warn!("fatal error reading id3v2 frame, skipping remainder of tag");
                break;
            }
        };
    }

    // Combine the v2.2/v2.3 year, date, and time frames into a single recording date.
    if let Some(tag) = legacy_dates.finish() {
        metadata.add_tag(tag);
    }

    // Compile table of contents and chapter frames into a set of Cues.
    if let Some(chapters) = chap_builder.build() {
        side_data.push(MetadataSideData::Chapters(chapters));
    }

    Ok(())
}

/// Read up to `len` bytes of tag data. Fewer bytes are returned if the stream ends first, so that
/// the frames of a truncated tag can still be read.
fn read_tag_data<B: ReadBytes>(reader: &mut B, len: u64) -> Result<Vec<u8>> {
    // The size of the tag is untrusted. Grow the buffer as data is actually read instead of
    // allocating it all up-front.
    const CHUNK_LEN: usize = 64 * 1024;

    let mut data = Vec::new();
    let mut remaining = len;

    while remaining > 0 {
        let start = data.len();
        let chunk_len = remaining.min(CHUNK_LEN as u64) as usize;

        data.resize(start + chunk_len, 0);

        let read = match reader.read_buf(&mut data[start..]) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(err) => return Err(err.into()),
        };

        data.truncate(start + read);

        // The end of the stream.
        if read == 0 {
            break;
        }

        remaining -= read as u64;
    }

    Ok(data)
}

pub(crate) fn read_id3v2<B: ReadBytes>(
    reader: &mut B,
    metadata: &mut MetadataBuilder,
    side_data: &mut Vec<MetadataSideData>,
) -> Result<()> {
    // Read the (sorta) version agnostic tag header.
    let header = read_id3v2_header(reader)?;

    // Read the tag. If the stream ends before the size of the tag is reached, the tag is truncated.
    // Keep the frames that are complete rather than discarding the whole tag.
    let mut data = read_tag_data(reader, u64::from(header.size))?;

    let is_truncated = (data.len() as u64) < u64::from(header.size);

    if is_truncated {
        warn!("id3v2 tag is truncated: {} of {} bytes", data.len(), header.size);
    }

    // If the unsynchronisation flag is set in the header, all tag data must be passed through the
    // unsynchronisation decoder before being read for verions < 4 of ID3v2. Otherwise, read the
    // data as-is. Individual frames may be unsynchronised for major versions >= 4.
    let body: &[u8] = if header.unsynchronisation && header.major_version < 4 {
        decode_unsynchronisation(&mut data)
    }
    else {
        &data
    };

    read_id3v2_body(body, &header, metadata, side_data)?;

    // If the header indicates the tag has a footer, it immediately follows the tag. The footer is
    // a copy of the header, so it carries no additional information. Skip it so that the reader is
    // positioned at the end of the tag. The tag has already been read at this point, so a
    // truncated footer is not an error.
    if header.has_footer && !is_truncated {
        if let Err(err) = reader.ignore_bytes(ID3V2_HEADER_LEN) {
            debug!("failed to skip id3v2 footer: {err}");
        }
    }

    Ok(())
}

/// An ID3v2 tag found at the end of a stream. See [`read_trailing_id3v2`].
pub struct TrailingId3v2 {
    /// The absolute byte position of the first byte of the tag (the "ID3" marker).
    pub start: u64,
    /// The absolute byte position immediately following the last byte of the tag (its footer, if
    /// it has one).
    pub end: u64,
    /// The decoded metadata.
    pub metadata: MetadataBuffer,
}

/// The length of an ID3v2 header or footer in bytes.
const ID3V2_HEADER_LEN: u64 = 10;

/// The length of an ID3v1 tag in bytes.
const ID3V1_TAG_LEN: u64 = 128;

/// The maximum number of bytes at the end of a stream that are searched for the header of an
/// appended ID3v2 tag that has no footer.
const TRAILING_SEARCH_LEN: u64 = 256 * 1024;

/// Read `buf.len()` bytes at the absolute position `pos`.
fn read_at(mss: &mut MediaSourceStream<'_>, pos: u64, buf: &mut [u8]) -> Option<()> {
    mss.seek(SeekFrom::Start(pos)).ok()?;
    mss.read_buf_exact(buf).ok()
}

/// If an APEv1 or APEv2 tag ends at `end`, get the position where it starts.
fn ape_tag_start(mss: &mut MediaSourceStream<'_>, end: u64) -> Option<u64> {
    let mut footer = [0u8; APE_HEADER_LEN as usize];
    read_at(mss, end.checked_sub(APE_HEADER_LEN)?, &mut footer)?;

    if &footer[..8] != b"APETAGEX" {
        return None;
    }

    // The size includes the footer and the items, but not the header.
    let size = u64::from(u32::from_le_bytes(footer[12..16].try_into().ok()?));
    let flags = ApeTagFlags::from_bits(u32::from_le_bytes(footer[20..24].try_into().ok()?));

    // The header flag would indicate this is the header itself, which is not valid for a footer.
    if flags.is_header || size < APE_HEADER_LEN {
        return None;
    }

    end.checked_sub(flags.tag_len(size))
}

/// Get the positions at which an ID3v2 tag appended to a stream of `len` bytes may end: the end of
/// the stream, before an ID3v1 tag, or before an APE tag (which may itself be before an ID3v1 tag).
fn trailing_tag_ends(mss: &mut MediaSourceStream<'_>, len: u64) -> Vec<u64> {
    let mut ends = vec![len];

    if let Some(pos) = len.checked_sub(ID3V1_TAG_LEN) {
        let mut marker = [0u8; 3];

        if read_at(mss, pos, &mut marker).is_some() && marker == *b"TAG" {
            ends.push(pos);
        }
    }

    for i in 0..ends.len() {
        if let Some(start) = ape_tag_start(mss, ends[i]) {
            ends.push(start);
        }
    }

    ends
}

/// If an ID3v2.4 tag with a footer ends at `end`, get the position where it starts.
fn id3v2_footer_tag_start(mss: &mut MediaSourceStream<'_>, end: u64) -> Option<u64> {
    // The smallest possible tag is a header and a footer.
    let footer_pos = end.checked_sub(ID3V2_HEADER_LEN)?;

    let mut footer = [0u8; ID3V2_HEADER_LEN as usize];
    read_at(mss, footer_pos, &mut footer)?;

    // The footer is the same as the header, but with the "3DI" marker.
    if footer[0..3] != *b"3DI" || footer[3] != 4 || footer[5] & 0x10 == 0 {
        return None;
    }

    // The size of the tag, excluding the header and footer, is syncsafe encoded.
    if footer[6..10].iter().any(|b| b & 0x80 != 0) {
        return None;
    }

    let size = footer[6..10].iter().fold(0u64, |size, &b| (size << 7) | u64::from(b));

    // Find the start of the tag, and ensure it exists.
    let start = footer_pos.checked_sub(size)?.checked_sub(ID3V2_HEADER_LEN)?;

    let mut header = [0u8; ID3V2_HEADER_LEN as usize];
    read_at(mss, start, &mut header)?;

    // The header must mirror the footer.
    (header[0..3] == *b"ID3" && header[3..] == footer[3..]).then_some(start)
}

/// Search the last `TRAILING_SEARCH_LEN` bytes of a stream of `len` bytes for the header of a tag
/// that ends at one of `ends`. Returns the position of the start and the end of the tag.
///
/// This is how an appended tag without a footer (ID3v2.2 and ID3v2.3 tags, and ID3v2.4 tags
/// without the optional footer) is found. It is unlikely that the bytes of any other data in the
/// stream would form a valid header with a size that ends exactly at the end of the stream.
fn id3v2_search_trailing_header(
    mss: &mut MediaSourceStream<'_>,
    len: u64,
    ends: &[u64],
) -> Option<(u64, u64)> {
    let buf_start = len.saturating_sub(TRAILING_SEARCH_LEN);

    let mut buf = vec![0u8; usize::try_from(len - buf_start).ok()?];
    read_at(mss, buf_start, &mut buf)?;

    // Search backwards so that the tag nearest to the end is preferred.
    let last = buf.len().checked_sub(ID3V2_HEADER_LEN as usize)?;

    for i in (0..=last).rev() {
        let hdr = &buf[i..i + ID3V2_HEADER_LEN as usize];

        let Some(hdr) = parse_header_bytes(hdr)
        else {
            continue;
        };

        let footer_len = if hdr.major >= 4 && hdr.flags & 0x10 != 0 { ID3V2_HEADER_LEN } else { 0 };

        let start = buf_start + i as u64;
        let end = start + ID3V2_HEADER_LEN + u64::from(hdr.size) + footer_len;

        if !ends.contains(&end) {
            continue;
        }

        return Some((start, end));
    }

    None
}

/// Whether the tag starting at `start` is the payload of an "ID3 " chunk, which the container
/// (RIFF/AIFF: 4-byte size after the ID; DSDIFF/CAF: 8-byte size) reads itself.
fn in_container_id3_chunk(mss: &mut MediaSourceStream<'_>, start: u64) -> bool {
    let Some(pos) = start.checked_sub(12)
    else {
        return start >= 8 && {
            let mut id = [0u8; 4];
            read_at(mss, start - 8, &mut id).is_some() && id.eq_ignore_ascii_case(b"id3 ")
        };
    };
    let mut hdr = [0u8; 12];
    read_at(mss, pos, &mut hdr).is_some()
        && (hdr[4..8].eq_ignore_ascii_case(b"id3 ") || hdr[0..4].eq_ignore_ascii_case(b"id3 "))
}

fn find_trailing_id3v2(mss: &mut MediaSourceStream<'_>, len: u64) -> Option<Range<u64>> {
    let ends = trailing_tag_ends(mss, len);

    // Prefer the tag closest to the end of the stream. A tag in a container's ID3 chunk is not an
    // appended tag.
    let found = ends
        .iter()
        .find_map(|&end| id3v2_footer_tag_start(mss, end).map(|start| start..end))
        .or_else(|| id3v2_search_trailing_header(mss, len, &ends).map(|(start, end)| start..end))?;

    (!in_container_id3_chunk(mss, found.start)).then_some(found)
}

/// Locate an ID3v2 tag that has been appended to the end of a stream, without reading it.
///
/// `stream_len` is the total length of the stream in bytes. The stream must be seekable. The
/// position of the stream is always restored before returning, regardless of the outcome. See
/// [`read_trailing_id3v2`] for how an appended tag is found.
///
/// Returns the range of bytes occupied by the tag, or `None` if one was not found.
pub fn locate_trailing_id3v2(
    mss: &mut MediaSourceStream<'_>,
    stream_len: u64,
) -> Result<Option<Range<u64>>> {
    if !mss.is_seekable() {
        return Ok(None);
    }

    let init_pos = mss.pos();

    let range = find_trailing_id3v2(mss, stream_len);

    // Restore the position of the stream.
    mss.seek(SeekFrom::Start(init_pos))?;

    Ok(range)
}

/// Locate and read an ID3v2 tag that has been appended to the end of a stream.
///
/// The tag may directly end the stream, or be followed by an ID3v1 tag and/or an APE tag. An
/// ID3v2.4 tag with a footer is found by the footer ("3DI") that mirrors the tag's header and gives
/// the size of the tag. Any other tag does not have a footer, so the last bytes of the stream are
/// searched for the header of a tag that ends exactly where the stream, or the tags that follow the
/// ID3v2 tag, begin.
///
/// `stream_len` is the total length of the stream in bytes. The stream must be seekable. The
/// position of the stream is always restored before returning, regardless of the outcome.
///
/// Returns `Ok(None)` if a tag was not found, or the tag could not be read.
pub fn read_trailing_id3v2(
    mss: &mut MediaSourceStream<'_>,
    stream_len: u64,
) -> Result<Option<TrailingId3v2>> {
    if !mss.is_seekable() {
        return Ok(None);
    }

    let init_pos = mss.pos();

    let result = read_located_trailing_id3v2(mss, stream_len);

    // Restore the position of the stream.
    mss.seek(SeekFrom::Start(init_pos))?;

    match result {
        Ok(tag) => Ok(tag),
        Err(err) => {
            debug!("failed to read trailing id3v2 tag: {err}");
            Ok(None)
        }
    }
}

fn read_located_trailing_id3v2(
    mss: &mut MediaSourceStream<'_>,
    stream_len: u64,
) -> Result<Option<TrailingId3v2>> {
    let Some(Range { start, end }) = find_trailing_id3v2(mss, stream_len)
    else {
        return Ok(None);
    };

    mss.seek(SeekFrom::Start(start))?;

    let mut builder = MetadataBuilder::new(ID3V2_METADATA_INFO);
    let mut side_data = Vec::new();

    read_id3v2(mss, &mut builder, &mut side_data)?;

    Ok(Some(TrailingId3v2 {
        start,
        end,
        metadata: MetadataBuffer { revision: builder.build(), side_data },
    }))
}

/// The chapter group builder utility validates and builds a `ChapterGroup` from a set of ID3v2
/// chapter and table of contents frames.
///
/// Constraints:
///
///  1. All table of contents and chapters shall be uniquely identified.
///  2. There shall only be 1 top-level table of contents.
///  3. A table of contents shall only be referenced once to prevent loops.
///  4. A table of contents that is not top-level must be referenced.
///  5. A chapter does not need to be referenced. However, if it is referenced, it may
///     only be referenced once.
///
/// Any constraint violation will halt processing and drop all cues.
#[derive(Default)]
struct ChapterGroupBuilder {
    is_err: bool,
    root_toc_id: Option<String>,
    tocs: HashMap<String, Id3v2TableOfContents>,
    chaps: HashMap<String, Id3v2Chapter>,
}

impl ChapterGroupBuilder {
    fn error(&mut self) {
        self.is_err = true;
        self.root_toc_id = None;
        self.tocs.clear();
        self.chaps.clear();
    }

    fn is_unique_key(&self, id: &str) -> bool {
        !(self.tocs.contains_key(id) || self.chaps.contains_key(id))
    }

    /// Add an ID3v2 table of contents.
    fn add_toc(&mut self, toc: Id3v2TableOfContents) {
        // Do not allow non-unique element IDs.
        if !self.is_unique_key(&toc.id) {
            debug!("id3v2: toc id '{}' is not unique", toc.id);
            return self.error();
        }

        // Do not allow multiple top-level table of contents.
        if toc.top_level {
            if self.root_toc_id.is_some() {
                debug!("id3v2: multiple top-level toc");
                return self.error();
            }
            self.root_toc_id = Some(toc.id.to_string())
        }

        self.tocs.insert(toc.id.to_string(), toc);
    }

    /// Add an ID3v2 chapter.
    fn add_chapter(&mut self, mut chap: Id3v2Chapter) {
        // Do not allow non-unique element IDs.
        if !self.is_unique_key(&chap.id) {
            debug!("id3v2: chapter id '{}' is not unique", chap.id);
            return self.error();
        }

        // Store the chapter count at the time this chapter was added to order standalone chapters
        // later by order read.
        chap.read_order = self.chaps.len();

        self.chaps.insert(chap.id.to_string(), chap);
    }

    /// Build a chapter group from the added ID3v2 table of contents and chapters.
    fn build(mut self) -> Option<ChapterGroup> {
        /// The maximum table of contents nesting depth. Elements beyond this depth are treated
        /// as broken references so that a deeply chained table of contents cannot exhaust the
        /// call stack via unbounded recursion.
        const MAX_TOC_DEPTH: usize = 32;

        /// Depth-first traversal of TOC elements to build the chapter hierarchy.
        fn dfs(
            id: &str,
            tocs: &mut HashMap<String, Id3v2TableOfContents>,
            chaps: &mut HashMap<String, Id3v2Chapter>,
            depth: usize,
        ) -> Option<ChapterGroup> {
            if depth == 0 {
                debug!("id3v2: maximum toc nesting depth exceeded");
                return None;
            }

            // Attempt to remove the TOC with the provided TOC ID. If it was removed, create a
            // chapter group from it.
            tocs.remove(id).map(|toc| {
                let mut parent = ChapterGroup {
                    items: Vec::with_capacity(toc.items.len()),
                    tags: toc.tags,
                    visuals: toc.visuals,
                };

                for child_id in toc.items {
                    if let Some(chap) = chaps.remove(&child_id) {
                        // Item is a chapter.
                        parent.items.push(ChapterGroupItem::Chapter(chap.chapter));
                    }
                    else if let Some(child) = dfs(&child_id, tocs, chaps, depth - 1) {
                        // Item is a TOC.
                        parent.items.push(ChapterGroupItem::Group(child));
                    }
                    else {
                        // Item reference is broken.
                        debug!("id3v2: missing chapter or toc element");
                    }
                }

                parent
            })
        }

        // Can't build if an error was found.
        if self.is_err {
            return None;
        }

        // Build a chapter group starting at top-level table of contents (TOC), if one exists.
        let top = self
            .root_toc_id
            .and_then(|toc_id| dfs(&toc_id, &mut self.tocs, &mut self.chaps, MAX_TOC_DEPTH));

        // After building the chapter group from the top-level TOC, any remaining TOC elements are
        // unreferenced. The behaviour in this case is not specified by the standard. Therefore,
        // these TOC and the chapters referenced by them will be considered invalid and removed.
        if !self.tocs.is_empty() {
            debug!("id3v2: unreferenced toc and/or chapter elements");

            for toc in self.tocs.values() {
                for child_id in &toc.items {
                    self.chaps.remove(child_id);
                }
            }

            self.tocs.clear();
        }

        // The remaining chapters are completely unreferenced. This is allowed per the ID3v2
        // specification. To accomodate these chapters, a new top-level TOC is created with the
        // original top-level nested underneath it with all the unreferenced chapters as siblings.
        if !self.chaps.is_empty() {
            // Collect all standalone ID3v2 chapters.
            let mut items: Vec<Id3v2Chapter> = self.chaps.into_values().collect();

            // Sort by read order.
            items.sort_by_key(|item| item.read_order);

            // Convert to chapter group items.
            let mut items: Vec<ChapterGroupItem> =
                items.into_iter().map(|chap| ChapterGroupItem::Chapter(chap.chapter)).collect();

            // Add the top-level chapter group, if one exists.
            if let Some(top) = top {
                items.push(ChapterGroupItem::Group(top));
            }

            Some(ChapterGroup { items, tags: vec![], visuals: vec![] })
        }
        else {
            top
        }
    }
}

pub(crate) const ID3V2_METADATA_INFO: MetadataInfo =
    MetadataInfo { metadata: METADATA_ID_ID3V2, short_name: "id3v2", long_name: "ID3v2" };

/// ID3v2 tag reader.
pub struct Id3v2Reader<'s> {
    reader: MediaSourceStream<'s>,
}

impl<'s> Id3v2Reader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, _opts: MetadataOptions) -> Result<Self> {
        Ok(Self { reader: mss })
    }
}

impl Scoreable for Id3v2Reader<'_> {
    /// Accept only a structurally valid tag header, so `ID3` bytes inside junk, padding or picture
    /// data are not mistaken for a tag (which would fail reading and hide the audio after it).
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut hdr = [0u8; 10];
        src.read_buf_exact(&mut hdr)?;

        let valid = parse_header_bytes(&hdr).is_some_and(|hdr| hdr.is_supported());

        Ok(if valid { Score::Supported(255) } else { Score::Unsupported })
    }
}

impl ProbeableMetadata<'_> for Id3v2Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: MetadataOptions,
    ) -> Result<Box<dyn MetadataReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(Id3v2Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeMetadataData] {
        &[support_metadata!(ID3V2_METADATA_INFO, &[], &[], &[b"ID3"])]
    }
}

impl MetadataReader for Id3v2Reader<'_> {
    fn metadata_info(&self) -> &MetadataInfo {
        &ID3V2_METADATA_INFO
    }

    fn read_all(&mut self) -> Result<MetadataBuffer> {
        let mut builder = MetadataBuilder::new(ID3V2_METADATA_INFO);
        let mut side_data = Vec::new();

        read_id3v2(&mut self.reader, &mut builder, &mut side_data)?;

        Ok(MetadataBuffer { revision: builder.build(), side_data })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

/// ID3v2 reader for a tag that has been appended to the end of a stream.
///
/// Unlike [`Id3v2Reader`], this reader is not found by a marker. Instead, when registered with a
/// [`Probe`](symphonia_core::formats::probe::Probe), it is run once while probing for trailing
/// metadata, and locates the tag relative to the end of the stream (see [`read_trailing_id3v2`]).
/// This allows the metadata of any container format to include an appended ID3v2 tag.
///
/// The stream must be seekable and have a known length, otherwise no metadata is read.
pub struct Id3v2TrailingReader<'s> {
    reader: MediaSourceStream<'s>,
}

impl<'s> Id3v2TrailingReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, _opts: MetadataOptions) -> Result<Self> {
        Ok(Self { reader: mss })
    }
}

impl Scoreable for Id3v2TrailingReader<'_> {
    /// The reader has no markers, so is never scored.
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Unsupported)
    }
}

impl ProbeableMetadata<'_> for Id3v2TrailingReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: MetadataOptions,
    ) -> Result<Box<dyn MetadataReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(Id3v2TrailingReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeMetadataData] {
        &[support_metadata!(ID3V2_METADATA_INFO, &[], &[], &[], Anchors::TrailingScan)]
    }
}

impl MetadataReader for Id3v2TrailingReader<'_> {
    fn metadata_info(&self) -> &MetadataInfo {
        &ID3V2_METADATA_INFO
    }

    fn read_all(&mut self) -> Result<MetadataBuffer> {
        let len = match self.reader.byte_len() {
            Some(len) if self.reader.is_seekable() => len,
            _ => return unsupported_error("id3v2: the stream is not seekable or has no length"),
        };

        match read_trailing_id3v2(&mut self.reader, len)? {
            // A tag at the start of the stream is a prepended tag, which is found by the probe
            // when it scans from the start of the stream.
            Some(tag) if tag.start > 0 => {
                debug!("found a trailing id3v2 tag @ {}..{}", tag.start, tag.end);
                Ok(tag.metadata)
            }
            _ => unsupported_error("id3v2: no trailing tag"),
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

pub mod sub_fields {
    //! Key name constants for sub-fields of well-known ID3v2 frames.
    //!
    //! For the exact meaning of these fields, and the format of their values, please consult the
    //! official ID3v2 specification.

    // Generally applicable to all frames

    pub const ENCRYPTION_METHOD_ID: &str = "ENCRYPTION_METHOD_ID";
    pub const GROUP_ID: &str = "GROUP_ID";

    // AENC frames

    pub const AENC_OWNER: &str = "OWNER";
    pub const AENC_PREVIEW_LENGTH: &str = "PREVIEW_LEN";
    pub const AENC_PREVIEW_START: &str = "PREVIEW_START";

    // ATXT frames

    pub const ATXT_EQUIVALENT_TEXT: &str = "EQUIVALENT_TEXT";
    pub const ATXT_MIME_TYPE: &str = "MIME_TYPE";

    // COMM frames

    pub const COMM_LANGUAGE: &str = "LANGUAGE";
    pub const COMM_SHORT_DESCRIPTION: &str = "SHORT_DESCRIPTION";

    // COMR frames

    pub const COMR_CONTACT_URL: &str = "CONTACT_URL";
    pub const COMR_DESCRIPTION: &str = "DESCRIPTION";
    pub const COMR_MIME_TYPE: &str = "MIME_TYPE";
    pub const COMR_RECEIVED_AS: &str = "RECEIVED_AS";
    pub const COMR_SELLER_LOGO: &str = "SELLER_LOGO";
    pub const COMR_SELLER_NAME: &str = "SELLER_NAME";
    pub const COMR_VALID_UNTIL: &str = "VALID_UNTIL";

    // CRM frames

    pub const CRM_OWNER: &str = "OWNER";
    pub const CRM_DESRIPTION: &str = "DESCRIPTION";

    // ENCR frames

    pub const ENCR_ENCRYPTION_DATA: &str = "ENCRYPTION_DATA";
    pub const ENCR_OWNER: &str = "OWNER";

    // GEOB frames

    pub const GEOB_DESCRIPTION: &str = "DESCRIPTION";
    pub const GEOB_FILE_NAME: &str = "FILE_NAME";
    pub const GEOB_MIME_TYPE: &str = "MIME_TYPE";

    // GRID frames

    pub const GRID_GROUP_DATA: &str = "GROUP_DATA";
    pub const GRID_OWNER: &str = "OWNER";
    pub const OWNE_PURCHASE_DATE: &str = "PURCHASE_DATE";
    pub const OWNE_SELLER_NAME: &str = "SELLER_NAME";

    // POPM frames

    pub const POPM_EMAIL: &str = "EMAIL";
    pub const POPM_PLAY_COUNTER: &str = "PLAY_COUNTER";

    // POSS frames

    pub const POSS_POSITION_UNITS: &str = "UNITS";

    // PRIV frames

    pub const PRIV_OWNER: &str = "OWNER";

    // SIGN frames

    pub const SIGN_GROUP_ID: &str = "GROUP_ID";

    // TXXX frames

    pub const TXXX_DESCRIPTION: &str = "DESCRIPTION";

    // UFID frames

    pub const UFID_OWNER: &str = "OWNER";

    // USER frames

    pub const USER_LANGUAGE: &str = "LANGUAGE";

    // USLT frames

    pub const USLT_LANGUAGE: &str = "LANGUAGE";
    pub const USLT_DESCRIPTION: &str = "DESCRIPTION";

    // WXXX frames

    pub const WXXX_DESCRIPTION: &str = "DESCRIPTION";
}

#[cfg(test)]
mod tests {
    use super::unsync::encode_synchsafe;
    use super::*;

    use std::sync::Arc;

    use std::io::Cursor;
    use symphonia_core::meta::{StandardTag, Tag};

    /// Generate an ID3v2.3 tag containing a linear chain of `count` CTOC elements where each
    /// element references the next. The first element is marked as top-level.
    fn toc_chain(count: usize) -> Vec<u8> {
        let mut frames = Vec::new();
        for i in 0..count {
            let mut body = format!("t{:06}", i).into_bytes();
            body.push(0);
            body.push(if i == 0 { 2 } else { 0 });
            body.push(u8::from(i + 1 < count));
            if i + 1 < count {
                body.extend_from_slice(format!("t{:06}", i + 1).as_bytes());
                body.push(0);
            }
            frames.extend_from_slice(b"CTOC");
            frames.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frames.extend_from_slice(&[0, 0]);
            frames.extend_from_slice(&body);
        }

        let mut tag = b"ID3\x03\x00\x00".to_vec();
        tag.extend_from_slice(&encode_synchsafe(frames.len() as u32));
        tag.extend_from_slice(&frames);
        tag
    }

    fn read_chapters(tag: Vec<u8>) -> Option<ChapterGroup> {
        let mss =
            MediaSourceStream::new(Box::new(Cursor::new(tag)), MediaSourceStreamOptions::default());
        let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).unwrap();
        reader.read_all().unwrap().side_data.iter().find_map(|sd| match sd {
            MetadataSideData::Chapters(group) => Some(group.clone()),
            _ => None,
        })
    }

    fn group_depth(group: &ChapterGroup) -> usize {
        group
            .items
            .iter()
            .map(|item| match item {
                ChapterGroupItem::Group(child) => 1 + group_depth(child),
                ChapterGroupItem::Chapter(_) => 1,
            })
            .max()
            .unwrap_or(0)
    }

    /// A deeply nested table of contents must not cause unbounded recursion. Elements beyond
    /// the maximum nesting depth are dropped.
    #[test]
    fn verify_deep_toc_chain_is_bounded() {
        let group = read_chapters(toc_chain(10_000)).expect("expected a chapter group");
        // The group contains 32 nested levels; the innermost level is empty.
        assert_eq!(group_depth(&group), 31);
    }

    /// A normally nested table of contents builds the full hierarchy.
    #[test]
    fn verify_nested_toc_chain() {
        let group = read_chapters(toc_chain(4)).expect("expected a chapter group");
        // A chain of 4 elements nests 4 groups; the innermost group is empty.
        assert_eq!(group_depth(&group), 3);
    }

    /// An ID3v2.3 frame with a plain 32-bit size.
    fn frame_v23(id: &[u8; 4], flags: u16, body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    /// An ID3v2.4 frame with a syncsafe size.
    fn frame_v24(id: &[u8; 4], flags: u16, body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&encode_synchsafe(body.len() as u32));
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    /// A UTF-8 (encoding 3) text frame body with NUL-separated values.
    fn utf8_text(values: &[&str]) -> Vec<u8> {
        let mut b = vec![3u8];
        b.extend_from_slice(values.join("\0").as_bytes());
        b
    }

    fn tag_bytes(major: u8, frames: &[u8], footer: bool) -> Vec<u8> {
        let flags = if footer { 0x10 } else { 0 };
        let mut tag = vec![b'I', b'D', b'3', major, 0, flags];
        tag.extend_from_slice(&encode_synchsafe(frames.len() as u32));
        tag.extend_from_slice(frames);

        if footer {
            tag.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
            tag.extend_from_slice(&encode_synchsafe(frames.len() as u32));
        }

        tag
    }

    fn read_tags(tag: Vec<u8>) -> Vec<Tag> {
        let mss =
            MediaSourceStream::new(Box::new(Cursor::new(tag)), MediaSourceStreamOptions::default());
        let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).unwrap();
        reader.read_all().unwrap().revision.media.tags
    }

    /// Collect the values of all standard tags selected by `f`.
    fn collect_std(tags: &[Tag], f: impl Fn(&StandardTag) -> Option<&Arc<String>>) -> Vec<String> {
        tags.iter().filter_map(|t| t.std.as_ref().and_then(&f)).map(|v| v.to_string()).collect()
    }

    fn artists(tags: &[Tag]) -> Vec<String> {
        collect_std(tags, |s| match s {
            StandardTag::Artist(v) => Some(v),
            _ => None,
        })
    }

    fn genres(tags: &[Tag]) -> Vec<String> {
        collect_std(tags, |s| match s {
            StandardTag::Genre(v) => Some(v),
            _ => None,
        })
    }

    fn titles(tags: &[Tag]) -> Vec<String> {
        collect_std(tags, |s| match s {
            StandardTag::TrackTitle(v) => Some(v),
            _ => None,
        })
    }

    /// Latin-1 (encoding 0) text containing Windows-1252 bytes is decoded as Windows-1252.
    #[test]
    fn verify_latin1_text_is_decoded_as_windows_1252() {
        let mut body = vec![0u8];
        body.extend_from_slice(b"It\x92s a \x93test\x94");
        let frames = frame_v23(b"TIT2", 0, &body);

        let tags = read_tags(tag_bytes(3, &frames, false));

        assert_eq!(titles(&tags), ["It\u{2019}s a \u{201c}test\u{201d}"]);
    }

    /// "UTF-16 with BOM" text without a BOM is decoded using a guess of its byte order.
    #[test]
    fn verify_utf16_without_bom_guesses_byte_order() {
        let le: Vec<u8> = "Tést".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = "Tést".encode_utf16().flat_map(u16::to_be_bytes).collect();

        for text in [le, be] {
            let mut body = vec![1u8];
            body.extend_from_slice(&text);
            let frames = frame_v23(b"TIT2", 0, &body);

            let tags = read_tags(tag_bytes(3, &frames, false));

            assert_eq!(titles(&tags), ["Tést"]);
        }
    }

    /// Bytes undefined in Windows-1252 decode to the replacement character.
    #[test]
    fn verify_latin1_undefined_windows_1252_byte() {
        let frames = frame_v23(b"TIT2", 0, b"\0a\x81b");

        let tags = read_tags(tag_bytes(3, &frames, false));

        assert_eq!(titles(&tags), ["a\u{fffd}b"]);
    }

    /// A multi-valued frame yields one standard tag per value, and genre references are resolved.
    #[test]
    fn verify_multi_valued_frames_are_mapped() {
        let mut frames = frame_v24(b"TPE1", 0, &utf8_text(&["Artist A", "Artist B", "Artist C"]));
        frames.extend(frame_v24(
            b"TCON",
            0,
            &utf8_text(&["(13)", "(17)Rock", "RX", "CR", "Ambient"]),
        ));

        let tags = read_tags(tag_bytes(4, &frames, false));

        assert_eq!(artists(&tags), ["Artist A", "Artist B", "Artist C"]);
        assert_eq!(genres(&tags), ["Pop", "Rock", "Remix", "Cover", "Ambient"]);
    }

    /// A UFID frame owned by MusicBrainz is mapped to the recording ID. Other owners are not.
    #[test]
    fn verify_ufid_musicbrainz_recording_id() {
        let mbid = "8622e4d1-bc90-4532-b8df-35f6bbb6731c";

        let mut body = b"http://musicbrainz.org\0".to_vec();
        body.extend_from_slice(mbid.as_bytes());

        let mut other = b"http://example.org\0".to_vec();
        other.extend_from_slice(mbid.as_bytes());

        let mut frames = frame_v24(b"UFID", 0, &body);
        frames.extend(frame_v24(b"UFID", 0, &other));

        let tags = read_tags(tag_bytes(4, &frames, false));

        let ids: Vec<_> = tags
            .iter()
            .filter_map(|t| match &t.std {
                Some(StandardTag::MusicBrainzRecordingId(v)) => Some(v.to_string()),
                _ => None,
            })
            .collect();

        assert_eq!(ids, [mbid]);
        assert_eq!(tags.len(), 2);
    }

    // zlib.compress(b"\x03Compressed Title Compressed Title", 9)
    const ZLIB_TITLE: [u8; 28] = [
        0x78, 0xda, 0x63, 0x76, 0xce, 0xcf, 0x2d, 0x28, 0x4a, 0x2d, 0x2e, 0x4e, 0x4d, 0x51, 0x08,
        0xc9, 0x2c, 0xc9, 0x49, 0x55, 0x40, 0x17, 0x00, 0x00, 0xd6, 0x77, 0x0c, 0x92,
    ];
    const ZLIB_TITLE_LEN: u32 = 34;

    /// A zlib-compressed ID3v2.3 frame is decompressed.
    #[test]
    fn verify_compressed_frame_v23() {
        let mut body = ZLIB_TITLE_LEN.to_be_bytes().to_vec();
        body.extend_from_slice(&ZLIB_TITLE);

        let mut frames = frame_v23(b"TIT2", 0x0080, &body);
        frames.extend(frame_v23(b"TPE1", 0, &utf8_text(&["After"])));

        let tags = read_tags(tag_bytes(3, &frames, false));

        assert_eq!(titles(&tags), ["Compressed Title Compressed Title"]);
        // Frames following the compressed frame are still read.
        assert_eq!(artists(&tags), ["After"]);
    }

    /// A zlib-compressed ID3v2.4 frame with a data length indicator is decompressed.
    #[test]
    fn verify_compressed_frame_v24() {
        let mut body = encode_synchsafe(ZLIB_TITLE_LEN).to_vec();
        body.extend_from_slice(&ZLIB_TITLE);

        let mut frames = frame_v24(b"TIT2", 0x0009, &body);
        frames.extend(frame_v24(b"TPE1", 0, &utf8_text(&["After"])));

        let tags = read_tags(tag_bytes(4, &frames, false));

        assert_eq!(titles(&tags), ["Compressed Title Compressed Title"]);
        assert_eq!(artists(&tags), ["After"]);
    }

    /// A compressed frame that decompresses to more than its indicated size, or that is corrupt,
    /// is skipped without affecting the remainder of the tag.
    #[test]
    fn verify_compressed_frame_limits() {
        // Indicated size is too small.
        let mut small = 4u32.to_be_bytes().to_vec();
        small.extend_from_slice(&ZLIB_TITLE);

        // Corrupt zlib stream.
        let mut corrupt = ZLIB_TITLE_LEN.to_be_bytes().to_vec();
        corrupt.extend_from_slice(&ZLIB_TITLE[..12]);

        let mut frames = frame_v23(b"TIT2", 0x0080, &small);
        frames.extend(frame_v23(b"TIT3", 0x0080, &corrupt));
        frames.extend(frame_v23(b"TPE1", 0, &utf8_text(&["After"])));

        let tags = read_tags(tag_bytes(3, &frames, false));

        assert!(titles(&tags).is_empty());
        assert_eq!(artists(&tags), ["After"]);
    }

    fn read_trailing(stream: Vec<u8>) -> Option<TrailingId3v2> {
        let len = stream.len() as u64;
        let mut mss = MediaSourceStream::new(
            Box::new(Cursor::new(stream)),
            MediaSourceStreamOptions::default(),
        );
        // Move to an arbitrary position to verify it is restored.
        mss.seek(SeekFrom::Start(3)).unwrap();
        let tag = read_trailing_id3v2(&mut mss, len).unwrap();
        assert_eq!(mss.pos(), 3);
        tag
    }

    /// An ID3v2.4 tag with a footer appended to the end of a stream is found.
    #[test]
    fn verify_trailing_id3v2() {
        let audio = vec![0xffu8; 700];
        let frames = frame_v24(b"TIT2", 0, &utf8_text(&["Trailing"]));
        let tag = tag_bytes(4, &frames, true);

        let mut stream = audio.clone();
        stream.extend_from_slice(&tag);

        let found = read_trailing(stream.clone()).expect("expected a trailing tag");
        assert_eq!(found.start, audio.len() as u64);
        assert_eq!(found.end, stream.len() as u64);
        assert_eq!(titles(&found.metadata.revision.media.tags), ["Trailing"]);

        // With an ID3v1 tag following.
        let mut v1 = vec![0u8; 128];
        v1[..3].copy_from_slice(b"TAG");
        stream.extend_from_slice(&v1);

        let found = read_trailing(stream).expect("expected a trailing tag before ID3v1");
        assert_eq!(found.start, audio.len() as u64);
        assert_eq!(found.end, (audio.len() + tag.len()) as u64);
        assert_eq!(titles(&found.metadata.revision.media.tags), ["Trailing"]);
    }

    /// Streams that do not end with a valid tag yield nothing.
    #[test]
    fn verify_trailing_id3v2_absent_or_invalid() {
        assert!(read_trailing(Vec::new()).is_none());
        assert!(read_trailing(vec![0u8; 300]).is_none());

        // A tag without a footer that does not end where the stream ends.
        let frames = frame_v24(b"TIT2", 0, &utf8_text(&["Not at the end"]));
        let mut stream = vec![0xffu8; 100];
        stream.extend(tag_bytes(4, &frames, false));
        stream.push(0);
        assert!(read_trailing(stream).is_none());

        // A tag in the "id3 " chunk at the end of a RIFF stream is read by the container.
        let tag = tag_bytes(3, &frame_v23(b"TIT2", 0, &utf8_text(&["Chunk"])), false);
        let mut stream = vec![0xffu8; 100];
        stream.extend_from_slice(b"id3 ");
        stream.extend_from_slice(&(tag.len() as u32).to_le_bytes());
        stream.extend(tag);
        assert!(read_trailing(stream).is_none());

        // A footer that claims a size larger than the stream.
        let mut stream = vec![0xffu8; 20];
        stream.extend_from_slice(&[b'3', b'D', b'I', 4, 0, 0x10, 0x7f, 0x7f, 0x7f, 0x7f]);
        assert!(read_trailing(stream).is_none());

        // A footer without a matching header.
        let mut stream = vec![0xffu8; 100];
        stream.extend_from_slice(&[b'3', b'D', b'I', 4, 0, 0x10, 0, 0, 0, 0x10]);
        assert!(read_trailing(stream).is_none());
    }

    /// The footer of a prepended tag is consumed, leaving the stream at the end of the tag.
    #[test]
    fn verify_footer_is_skipped() {
        for major in [4u8, 3] {
            let frames = if major == 4 {
                frame_v24(b"TIT2", 0, &utf8_text(&["Title"]))
            }
            else {
                frame_v23(b"TIT2", 0, &utf8_text(&["Title"]))
            };

            // A footer only exists in ID3v2.4 tags.
            let tag = tag_bytes(major, &frames, major == 4);
            let tag_len = tag.len() as u64;

            let mut stream = tag;
            stream.extend_from_slice(b"AUDIO");

            let mss = MediaSourceStream::new(
                Box::new(Cursor::new(stream)),
                MediaSourceStreamOptions::default(),
            );
            let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).unwrap();
            assert_eq!(titles(&reader.read_all().unwrap().revision.media.tags), ["Title"]);

            let mut mss = Box::new(reader).into_inner();
            assert_eq!(mss.pos(), tag_len);
            assert_eq!(mss.read_quad_bytes().unwrap(), *b"AUDI");
        }
    }

    /// A tag with an extended header of the given kind, followed by a TIT2 frame.
    fn tag_with_extended_header(major: u8, ext: &[u8], title: &str) -> Vec<u8> {
        let mut body = ext.to_vec();

        body.extend(if major == 4 {
            frame_v24(b"TIT2", 0, &utf8_text(&[title]))
        }
        else {
            frame_v23(b"TIT2", 0, &utf8_text(&[title]))
        });

        let mut tag = vec![b'I', b'D', b'3', major, 0, 0x40];
        tag.extend_from_slice(&encode_synchsafe(body.len() as u32));
        tag.extend(body);
        tag
    }

    /// An ID3v2.4 extended header with the update flag. The flag has no data (a length of 0), but
    /// some writers give it a data byte.
    #[test]
    fn verify_id3v24_extended_header_update_flag() {
        for flag_data in [&[0u8][..], &[1u8, 0u8][..]] {
            let mut ext = vec![1, 0x40];
            ext.extend_from_slice(flag_data);

            let mut header = encode_synchsafe(4 + ext.len() as u32).to_vec();
            header.extend(ext);

            let tags = read_tags(tag_with_extended_header(4, &header, "Updated"));
            assert_eq!(titles(&tags), ["Updated"]);
        }
    }

    /// The declared size of an ID3v2.4 extended header is used to find the first frame, regardless
    /// of the fields in the header.
    #[test]
    fn verify_id3v24_extended_header_size_is_used() {
        // CRC32 and restrictions flags, followed by 3 bytes the reader does not know about.
        let mut ext = vec![1, 0x30, 5, 0, 0, 0, 0, 0, 1, 0x00, 0xaa, 0xbb, 0xcc];
        let mut header = encode_synchsafe(4 + ext.len() as u32).to_vec();
        header.append(&mut ext);

        let tags = read_tags(tag_with_extended_header(4, &header, "Sized"));
        assert_eq!(titles(&tags), ["Sized"]);

        // A malformed field does not prevent the frames from being read.
        let mut ext = vec![1, 0x20, 9, 0, 0, 0, 0, 0];
        let mut header = encode_synchsafe(4 + ext.len() as u32).to_vec();
        header.append(&mut ext);

        let tags = read_tags(tag_with_extended_header(4, &header, "Malformed"));
        assert_eq!(titles(&tags), ["Malformed"]);
    }

    /// An ID3v2.3 extended header with a size of 10 has space for a CRC32 even if the CRC32 flag is
    /// not set.
    #[test]
    fn verify_id3v23_extended_header_without_crc() {
        let ext = [0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef];
        let tags = read_tags(tag_with_extended_header(3, &ext, "No CRC"));
        assert_eq!(titles(&tags), ["No CRC"]);

        // With the CRC32 flag set.
        let ext = [0, 0, 0, 10, 0x80, 0, 0, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef];
        let tags = read_tags(tag_with_extended_header(3, &ext, "CRC"));
        assert_eq!(titles(&tags), ["CRC"]);

        // The short extended header.
        let ext = [0, 0, 0, 6, 0, 0, 0, 0, 0, 0];
        let tags = read_tags(tag_with_extended_header(3, &ext, "Short"));
        assert_eq!(titles(&tags), ["Short"]);
    }

    /// An ID3v2.4 frame with a plain big-endian size instead of a syncsafe size.
    fn frame_v24_plain(id: &[u8; 4], flags: u16, body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    fn albums(tags: &[Tag]) -> Vec<String> {
        collect_std(tags, |s| match s {
            StandardTag::Album(v) => Some(v),
            _ => None,
        })
    }

    /// Some taggers write the frame sizes of ID3v2.4 tags as plain integers instead of syncsafe
    /// integers. They are read if the syncsafe size does not end at a frame, but the plain size does.
    #[test]
    fn verify_id3v24_non_syncsafe_frame_sizes() {
        // The bodies have sizes where the two representations differ: 200 (the plain size has a
        // byte with its high bit set), and 256 (neither does, but the sizes are still different).
        let long_title = "t".repeat(199);
        let long_album = "a".repeat(255);

        let mut frames = frame_v24_plain(b"TIT2", 0, &utf8_text(&[&long_title]));
        frames.extend(frame_v24_plain(b"TALB", 0, &utf8_text(&[&long_album])));
        frames.extend(frame_v24_plain(b"TPE1", 0, &utf8_text(&["Artist"])));

        let tags = read_tags(tag_bytes(4, &frames, false));

        assert_eq!(titles(&tags), [long_title.as_str()]);
        assert_eq!(albums(&tags), [long_album.as_str()]);
        assert_eq!(artists(&tags), ["Artist"]);

        // The same frames with correct syncsafe sizes are still read as syncsafe.
        let mut frames = frame_v24(b"TIT2", 0, &utf8_text(&[&long_title]));
        frames.extend(frame_v24(b"TALB", 0, &utf8_text(&[&long_album])));
        frames.extend(frame_v24(b"TPE1", 0, &utf8_text(&["Artist"])));

        let tags = read_tags(tag_bytes(4, &frames, false));

        assert_eq!(titles(&tags), [long_title.as_str()]);
        assert_eq!(albums(&tags), [long_album.as_str()]);
        assert_eq!(artists(&tags), ["Artist"]);
    }

    /// A frame with unused flag bits set is skipped, and the frames after it are still read.
    #[test]
    fn verify_frame_with_unused_flag_bits_is_skipped() {
        for major in [3u8, 4] {
            let frame = |id: &[u8; 4], flags: u16, value: &str| {
                let body = utf8_text(&[value]);
                if major == 3 { frame_v23(id, flags, &body) } else { frame_v24(id, flags, &body) }
            };

            // Bits that are unused in both versions.
            let mut frames = frame(b"TIT2", 0x0090, "Bogus");
            frames.extend(frame(b"TPE1", 0, "Good"));
            frames.extend(frame(b"TALB", 0, "Album"));

            let tags = read_tags(tag_bytes(major, &frames, false));

            assert!(titles(&tags).is_empty());
            assert_eq!(artists(&tags), ["Good"]);
            assert_eq!(albums(&tags), ["Album"]);

            // The frame is the last frame in the tag.
            let mut frames = frame(b"TPE1", 0, "Good");
            frames.extend(frame(b"TIT2", 0x0090, "Bogus"));

            let tags = read_tags(tag_bytes(major, &frames, false));

            assert!(titles(&tags).is_empty());
            assert_eq!(artists(&tags), ["Good"]);
        }
    }

    /// A frame with unused flag bits and a size that is not plausible loses the structure of the
    /// tag, but the frames before it are kept.
    #[test]
    fn verify_frame_with_unused_flag_bits_and_bad_size_ends_tag() {
        let mut frames = frame_v23(b"TPE1", 0, &utf8_text(&["Before"]));
        frames.extend_from_slice(b"TIT2\x00\x00\x10\x00\x00\x10");
        frames.extend_from_slice(&utf8_text(&["After"]));

        let tags = read_tags(tag_bytes(3, &frames, false));

        assert!(titles(&tags).is_empty());
        assert_eq!(artists(&tags), ["Before"]);
    }

    /// A tag that declares a size larger than the stream keeps the frames that were read in full.
    #[test]
    fn verify_truncated_tag_keeps_earlier_frames() {
        for major in [3u8, 4] {
            let frame = |id: &[u8; 4], value: &str| {
                let body = utf8_text(&[value]);
                if major == 3 { frame_v23(id, 0, &body) } else { frame_v24(id, 0, &body) }
            };

            let mut frames = frame(b"TPE1", "Complete");
            frames.extend(frame(b"TIT2", "Cut short"));

            // End the stream part-way through the body of the last frame.
            let mut tag = tag_bytes(major, &frames, false);
            tag.truncate(tag.len() - 4);

            let tags = read_tags(tag);

            assert!(titles(&tags).is_empty());
            assert_eq!(artists(&tags), ["Complete"]);

            // End the stream part-way through the header of the last frame.
            let mut tag = tag_bytes(major, &frames, false);
            let cut = tag.len() - (frame(b"TIT2", "Cut short").len() - 6);
            tag.truncate(cut);

            let tags = read_tags(tag);

            assert!(titles(&tags).is_empty());
            assert_eq!(artists(&tags), ["Complete"]);
        }
    }

    /// An ID3v2.2 frame.
    fn frame_v22(id: &[u8; 3], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        f.extend_from_slice(body);
        f
    }

    /// The tag of the given major version, with a TIT2 (title) frame, and no footer.
    fn tag_without_footer(major: u8, title: &str) -> Vec<u8> {
        let body = match major {
            // ID3v2.2 does not support UTF-8 text, so use ISO-8859-1.
            2 => frame_v22(b"TT2", &[&[0u8][..], title.as_bytes()].concat()),
            3 => frame_v23(b"TIT2", 0, &utf8_text(&[title])),
            _ => frame_v24(b"TIT2", 0, &utf8_text(&[title])),
        };

        tag_bytes(major, &body, false)
    }

    /// A tag without a footer that has been appended to a stream is found for all versions, even
    /// when followed by an ID3v1 or an APE tag.
    #[test]
    fn verify_trailing_id3v2_without_footer() {
        let audio = vec![0xffu8; 700];

        let mut v1 = vec![0u8; 128];
        v1[..3].copy_from_slice(b"TAG");

        // An empty APEv2 tag, which is just a footer.
        let mut ape = b"APETAGEX".to_vec();
        ape.extend_from_slice(&2000u32.to_le_bytes());
        ape.extend_from_slice(&32u32.to_le_bytes());
        ape.extend_from_slice(&0u32.to_le_bytes());
        ape.extend_from_slice(&0u32.to_le_bytes());
        ape.extend_from_slice(&[0u8; 8]);

        // Stream suffixes that may follow the tag.
        let ape_v1 = [ape.as_slice(), v1.as_slice()].concat();
        let suffixes: [&[u8]; 4] = [&[], &v1, &ape, &ape_v1];

        for major in [2u8, 3, 4] {
            for suffix in suffixes {
                let tag = tag_without_footer(major, "Appended");

                let mut stream = audio.clone();
                stream.extend_from_slice(&tag);
                stream.extend_from_slice(suffix);

                let found = read_trailing(stream).expect("expected a trailing tag");
                assert_eq!(found.start, audio.len() as u64);
                assert_eq!(found.end, (audio.len() + tag.len()) as u64);
                assert_eq!(titles(&found.metadata.revision.media.tags), ["Appended"]);
            }
        }
    }

    /// The location of an appended tag is found without reading it.
    #[test]
    fn verify_locate_trailing_id3v2() {
        let audio = vec![0xffu8; 300];
        let tag = tag_without_footer(3, "Located");

        let mut stream = audio.clone();
        stream.extend_from_slice(&tag);

        let len = stream.len() as u64;
        let mut mss = MediaSourceStream::new(
            Box::new(Cursor::new(stream)),
            MediaSourceStreamOptions::default(),
        );

        let range = locate_trailing_id3v2(&mut mss, len).unwrap().expect("expected a tag");
        assert_eq!(range, audio.len() as u64..len);
        assert_eq!(mss.pos(), 0);
    }
}
