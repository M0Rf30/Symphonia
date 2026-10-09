// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ID3v2 metadata reader.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::formats::probe::{ProbeMetadataData, ProbeableMetadata, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::well_known::METADATA_ID_ID3V2;
use symphonia_core::meta::{
    ChapterGroup, ChapterGroupItem, MetadataBuffer, MetadataBuilder, MetadataInfo, MetadataOptions,
    MetadataReader, MetadataSideData,
};
use symphonia_core::support_metadata;

use log::{debug, trace, warn};

mod frames;
mod unsync;

use frames::*;
use unsync::{UnsyncStream, read_syncsafe_leq32};

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
fn read_id3v2p3_extended_header<B: ReadBytes>(reader: &mut B) -> Result<ExtendedHeader> {
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

    // CRC32 flag.
    if size == 10 && flags & 0x8000 != 0 {
        header.crc32 = Some(reader.read_be_u32()?);
    }

    Ok(header)
}

/// Read the extended header of an ID3v2.4 tag.
fn read_id3v2p4_extended_header<B: ReadBytes>(reader: &mut B) -> Result<ExtendedHeader> {
    let _size = read_syncsafe_leq32(reader, 28)?;

    if reader.read_u8()? != 1 {
        return decode_error("id3v2: extended flags should have a length of 1");
    }

    let flags = reader.read_u8()?;

    let mut header = ExtendedHeader {
        padding_size: None,
        crc32: None,
        is_update: Some(false),
        restrictions: None,
    };

    // Tag is an update flag.
    if flags & 0x40 != 0x0 {
        let len = reader.read_u8()?;
        if len != 1 {
            return decode_error("id3v2: is update extended flag has invalid size");
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

fn read_id3v2_body<B: ReadBytes + FiniteStream>(
    reader: &mut B,
    header: &Header,
    metadata: &mut MetadataBuilder,
    side_data: &mut Vec<MetadataSideData>,
) -> Result<()> {
    // If there is an extended header, read and parse it based on the major version of the tag.
    if header.has_extended_header {
        let extended = match header.major_version {
            3 => read_id3v2p3_extended_header(reader)?,
            4 => read_id3v2p4_extended_header(reader)?,
            _ => unreachable!(),
        };
        trace!("{:#?}", extended);
    }

    let min_frame_size = min_frame_size(header.major_version);

    let mut chap_builder: ChapterGroupBuilder = Default::default();

    loop {
        // Read frames based on the major version of the tag.
        let frame = match header.major_version {
            2 => read_id3v2p2_frame(reader),
            3 => read_id3v2p3_frame(reader),
            4 => read_id3v2p4_frame(reader),
            _ => break,
        };

        match frame {
            // The frame was skipped for some reason.
            Ok(FrameResult::Skipped) => (),
            // The padding has been reached, don't parse any further.
            Ok(FrameResult::Padding) => break,
            // A frame was parsed into a tag, add it to the tag collection.
            Ok(FrameResult::Tag(tag)) => {
                metadata.add_tag(tag);
            }
            // A frame was parsed into multiple tags, add them all to the tag collection.
            Ok(FrameResult::MultipleTags(tags)) => {
                for tag in tags {
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

        // Read frames until there is not enough bytes available in the ID3v2 tag for another frame.
        if reader.bytes_available() < min_frame_size {
            break;
        }
    }

    // Compile table of contents and chapter frames into a set of Cues.
    if let Some(chapters) = chap_builder.build() {
        side_data.push(MetadataSideData::Chapters(chapters));
    }

    Ok(())
}

pub(crate) fn read_id3v2<B: ReadBytes>(
    reader: &mut B,
    metadata: &mut MetadataBuilder,
    side_data: &mut Vec<MetadataSideData>,
) -> Result<()> {
    // Read the (sorta) version agnostic tag header.
    let header = read_id3v2_header(reader)?;

    // If the unsynchronisation flag is set in the header, all tag data must be passed through the
    // unsynchronisation decoder before being read for verions < 4 of ID3v2.
    let mut scoped = if header.unsynchronisation && header.major_version < 4 {
        let mut unsync = UnsyncStream::new(ScopedStream::new(reader, u64::from(header.size)));

        read_id3v2_body(&mut unsync, &header, metadata, side_data)?;

        unsync.into_inner()
    }
    // Otherwise, read the data as-is. Individual frames may be unsynchronised for major versions
    // >= 4.
    else {
        let mut scoped = ScopedStream::new(reader, u64::from(header.size));

        read_id3v2_body(&mut scoped, &header, metadata, side_data)?;

        scoped
    };

    // Ignore any remaining data in the tag.
    scoped.ignore()?;

    // If the header indicates the tag has a footer, it immediately follows the tag. The footer is
    // a copy of the header, so it carries no additional information. Skip it so that the reader is
    // positioned at the end of the tag. The tag has already been read at this point, so a
    // truncated footer is not an error.
    if header.has_footer {
        let reader = scoped.into_inner();

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
    /// The absolute byte position immediately following the last byte of the tag (its footer).
    pub end: u64,
    /// The decoded metadata.
    pub metadata: MetadataBuffer,
}

/// The length of an ID3v2 header or footer in bytes.
const ID3V2_HEADER_LEN: u64 = 10;

/// The length of an ID3v1 tag in bytes.
const ID3V1_TAG_LEN: u64 = 128;

/// Locate and read an ID3v2.4 tag that has been appended to the end of a stream.
///
/// An appended tag can be found because, unlike a prepended tag, it ends with a footer ("3DI")
/// that mirrors the tag's header and gives the size of the tag. If the stream ends with an ID3v1
/// tag, the footer is searched for immediately before it.
///
/// `stream_len` is the total length of the stream in bytes. The stream must be seekable. The
/// position of the stream is always restored before returning, regardless of the outcome.
///
/// Returns `Ok(None)` if the stream does not end with a valid ID3v2 tag with a footer, or the tag
/// could not be read.
pub fn read_trailing_id3v2(
    mss: &mut MediaSourceStream<'_>,
    stream_len: u64,
) -> Result<Option<TrailingId3v2>> {
    if !mss.is_seekable() {
        return Ok(None);
    }

    let init_pos = mss.pos();

    let result = find_trailing_id3v2(mss, stream_len);

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

fn find_trailing_id3v2(
    mss: &mut MediaSourceStream<'_>,
    stream_len: u64,
) -> Result<Option<TrailingId3v2>> {
    // The tag may be directly at the end of the stream, or it may be followed by an ID3v1 tag.
    let mut ends = [Some(stream_len), None];

    if stream_len >= ID3V1_TAG_LEN {
        mss.seek(SeekFrom::Start(stream_len - ID3V1_TAG_LEN))?;

        if mss.read_triple_bytes()? == *b"TAG" {
            ends[1] = Some(stream_len - ID3V1_TAG_LEN);
        }
    }

    // Prefer the tag closest to the end of the stream.
    for end in ends.into_iter().flatten() {
        // The smallest possible tag is a header and a footer.
        let Some(footer_pos) = end.checked_sub(ID3V2_HEADER_LEN)
        else {
            continue;
        };

        mss.seek(SeekFrom::Start(footer_pos))?;

        let mut footer = [0u8; ID3V2_HEADER_LEN as usize];
        mss.read_buf_exact(&mut footer)?;

        // The footer is the same as the header, but with the "3DI" marker.
        if footer[0..3] != *b"3DI" || footer[3] != 4 || footer[5] & 0x10 == 0 {
            continue;
        }

        // The size of the tag, excluding the header and footer, is syncsafe encoded.
        if footer[6..10].iter().any(|b| b & 0x80 != 0) {
            continue;
        }

        let size = footer[6..10].iter().fold(0u64, |size, &b| (size << 7) | u64::from(b & 0x7f));

        // Find the start of the tag, and ensure it exists.
        let Some(start) =
            footer_pos.checked_sub(size).and_then(|p| p.checked_sub(ID3V2_HEADER_LEN))
        else {
            continue;
        };

        mss.seek(SeekFrom::Start(start))?;

        let mut header = [0u8; ID3V2_HEADER_LEN as usize];
        mss.read_buf_exact(&mut header)?;

        // The header must mirror the footer.
        if header[0..3] != *b"ID3" || header[3..] != footer[3..] {
            continue;
        }

        mss.seek(SeekFrom::Start(start))?;

        let mut builder = MetadataBuilder::new(ID3V2_METADATA_INFO);
        let mut side_data = Vec::new();

        read_id3v2(mss, &mut builder, &mut side_data)?;

        return Ok(Some(TrailingId3v2 {
            start,
            end,
            metadata: MetadataBuffer { revision: builder.build(), side_data },
        }));
    }

    Ok(None)
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
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
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
        tag.extend_from_slice(&[
            ((frames.len() >> 21) & 0x7f) as u8,
            ((frames.len() >> 14) & 0x7f) as u8,
            ((frames.len() >> 7) & 0x7f) as u8,
            (frames.len() & 0x7f) as u8,
        ]);
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

    fn syncsafe(n: usize) -> [u8; 4] {
        [
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]
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
        f.extend_from_slice(&syncsafe(body.len()));
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
        tag.extend_from_slice(&syncsafe(frames.len()));
        tag.extend_from_slice(frames);

        if footer {
            tag.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
            tag.extend_from_slice(&syncsafe(frames.len()));
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
        let mut body = syncsafe(ZLIB_TITLE_LEN as usize).to_vec();
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

    /// Streams that do not end with a valid tag with a footer yield nothing.
    #[test]
    fn verify_trailing_id3v2_absent_or_invalid() {
        assert!(read_trailing(Vec::new()).is_none());
        assert!(read_trailing(vec![0u8; 300]).is_none());

        // A tag without a footer.
        let frames = frame_v24(b"TIT2", 0, &utf8_text(&["No footer"]));
        let mut stream = vec![0xffu8; 100];
        stream.extend(tag_bytes(4, &frames, false));
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
}
