// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! APEv2 tags appended to WavPack, Monkey's Audio and Musepack files.
//!
//! The tag lives at the end of the file, optionally followed by an ID3v1 tag. Writing removes the
//! existing APE tag (v1 or v2), appends the new one at the same place and leaves any ID3v1 tag
//! after it untouched. The audio is copied verbatim.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::ape::write::{self, APE_HEADER_SIZE, APE_PREAMBLE};

use crate::{Result, WriteOptions, copy_range, read_up_to, replace_file};

/// The size of an ID3v1 tag.
const ID3V1_SIZE: u64 = 128;

const FLAG_HAS_HEADER: u32 = 1 << 31;
const FLAG_IS_HEADER: u32 = 1 << 29;

/// The location of an APE tag in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TagRange {
    /// The offset of the first byte of the tag (its header if it has one).
    pub start: u64,
    /// The offset one past the last byte of the tag (its footer).
    pub end: u64,
}

/// Where the APE tag and the trailing ID3v1 tag are in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TailLayout {
    /// The APE tag at the end of the file, if there is one. If the file has several consecutive
    /// APE tags, this covers all of them.
    pub tag: Option<TagRange>,
    /// The file has an ID3v1 tag in its last 128 bytes.
    pub id3v1: bool,
    /// The length of the file.
    pub len: u64,
}

impl TailLayout {
    /// The offset where the audio (or whatever precedes the tags) ends.
    pub fn audio_end(&self) -> u64 {
        match self.tag {
            Some(tag) => tag.start,
            None if self.id3v1 => self.len - ID3V1_SIZE,
            None => self.len,
        }
    }

    /// The offset where the data that follows the APE tag (the ID3v1 tag, if any) starts.
    pub fn tail_start(&self) -> u64 {
        match self.tag {
            Some(tag) => tag.end,
            None => self.audio_end(),
        }
    }
}

/// Finds the APE tag at the end of `file`: the footer must be the last 32 bytes of the file, or
/// directly precede a trailing ID3v1 tag.
pub(crate) fn find_tag(file: &mut File) -> Result<TailLayout> {
    let len = file.seek(SeekFrom::End(0))?;

    let mut marker = [0u8; 3];

    let id3v1 = len >= ID3V1_SIZE && {
        file.seek(SeekFrom::Start(len - ID3V1_SIZE))?;
        read_up_to(file, &mut marker)? == 3 && &marker == b"TAG"
    };

    let mut end = if id3v1 { len - ID3V1_SIZE } else { len };
    let tail_end = end;
    let mut tag: Option<TagRange> = None;

    // Walk backwards over every APE tag directly before `end`.
    while let Some(range) = tag_ending_at(file, end)? {
        end = range.start;
        tag = Some(TagRange { start: range.start, end: tag.map_or(range.end, |t| t.end) });
    }

    debug_assert!(tag.map_or(true, |t| t.end == tail_end));

    Ok(TailLayout { tag, id3v1, len })
}

/// Parses the APE tag whose footer ends at `end`, if there is one.
fn tag_ending_at(file: &mut File, end: u64) -> Result<Option<TagRange>> {
    let footer_len = APE_HEADER_SIZE as u64;

    if end < footer_len {
        return Ok(None);
    }

    let mut footer = [0u8; APE_HEADER_SIZE];
    file.seek(SeekFrom::Start(end - footer_len))?;

    if read_up_to(file, &mut footer)? != APE_HEADER_SIZE || &footer[..8] != APE_PREAMBLE {
        return Ok(None);
    }

    let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    let size = u64::from(u32::from_le_bytes(footer[12..16].try_into().unwrap()));
    let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());

    // In an APEv1 tag the size covers the items and the footer, and there is no header. APEv2
    // adds the header (which is not part of the size) if the flag says so.
    let has_header = match version {
        1000 => false,
        2000 => flags & FLAG_HAS_HEADER != 0,
        _ => return Ok(None),
    };

    let total = size + if has_header { footer_len } else { 0 };

    if size < footer_len || total > end {
        return Ok(None);
    }

    let start = end - total;

    if has_header {
        let mut header = [0u8; APE_HEADER_SIZE];
        file.seek(SeekFrom::Start(start))?;

        if read_up_to(file, &mut header)? != APE_HEADER_SIZE || &header[..8] != APE_PREAMBLE {
            return Ok(None);
        }

        let flags = u32::from_le_bytes(header[20..24].try_into().unwrap());

        if flags & FLAG_IS_HEADER == 0 {
            return Ok(None);
        }
    }

    Ok(Some(TagRange { start, end }))
}

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    _opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| {
        let layout = find_tag(src)?;

        // Keep the type of the items that were external links.
        let locators = match layout.tag {
            Some(range) => {
                // A tag is parsed from memory, which is bounded by what the tag may hold.
                let mut old = vec![0u8; (range.end - range.start) as usize];
                src.seek(SeekFrom::Start(range.start))?;
                src.read_exact(&mut old)?;
                write::locator_keys(&old)
            }
            None => Vec::new(),
        };

        let tag = write::serialize(tags, visuals, &locators);

        copy_range(src, out, 0, layout.audio_end())?;
        out.write_all(&tag)?;
        copy_range(src, out, layout.tail_start(), layout.len - layout.tail_start())?;

        Ok(())
    })
}
