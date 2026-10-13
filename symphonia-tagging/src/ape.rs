// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! APEv2 tags appended to WavPack, Monkey's Audio, Musepack, OptimFROG, TAK, and raw AAC (ADTS) and
//! AC-3 / E-AC-3 streams.
//!
//! The tag lives at the end of the file, optionally followed by an ID3v1 tag. Writing removes the
//! existing APE tag (v1 or v2), appends the new one at the same place (before the ID3v1 tag, or at
//! the end of the file when a stream has no APE tag yet) and leaves any ID3v1 tag after it
//! untouched. A leading ID3v2 tag is not touched either. The audio is copied verbatim.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::ape::write::{
    self, APE_HEADER_SIZE, APE_PREAMBLE, FLAG_HAS_HEADER, FLAG_IS_HEADER,
};
use symphonia_metadata::id3v1::write::ID3V1_TAG_LEN;

use crate::{ApePolicy, Result, WriteOptions, copy_range, read_at, read_up_to, replace_file};

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
            None if self.id3v1 => self.len - ID3V1_TAG_LEN as u64,
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

/// Finds the APE tag(s) that end exactly at `end`, walking backwards over consecutive tags. The
/// returned range covers all of them.
pub(crate) fn find_tag_before(file: &mut File, end: u64) -> Result<Option<TagRange>> {
    let mut end = end;
    let mut tag: Option<TagRange> = None;

    while let Some(range) = tag_ending_at(file, end)? {
        end = range.start;
        tag = Some(TagRange { start: range.start, end: tag.map_or(range.end, |t| t.end) });
    }

    Ok(tag)
}

/// Finds the APE tag at the end of `file`: the footer must be the last 32 bytes of the file, or
/// directly precede a trailing ID3v1 tag.
pub(crate) fn find_tag(file: &mut File) -> Result<TailLayout> {
    let len = file.seek(SeekFrom::End(0))?;

    let mut marker = [0u8; 3];

    let id3v1 = len >= ID3V1_TAG_LEN as u64 && {
        file.seek(SeekFrom::Start(len - ID3V1_TAG_LEN as u64))?;
        read_up_to(file, &mut marker)? == 3 && &marker == b"TAG"
    };

    let end = if id3v1 { len - ID3V1_TAG_LEN as u64 } else { len };
    let tag = find_tag_before(file, end)?;

    debug_assert!(tag.is_none_or(|t| t.end == end));

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

    let word = |b: &[u8; APE_HEADER_SIZE], i: usize| {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    };

    let version = word(&footer, 8);
    let size = u64::from(word(&footer, 12));
    let flags = word(&footer, 20);

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

        if word(&header, 20) & FLAG_IS_HEADER == 0 {
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
                let old = read_at(src, range.start, range.end - range.start)?;
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

/// Writes an APEv2 tag to a stream according to `policy`.
pub(crate) fn write_with_policy(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
    policy: ApePolicy,
) -> Result<()> {
    match policy {
        ApePolicy::Write => write(path, tags, visuals, opts),
        ApePolicy::Remove => remove(path),
        ApePolicy::Keep => Ok(()),
    }
}

/// Removes the APE tag(s) at the end of the file. The file is not touched when it has none.
pub(crate) fn remove(path: &Path) -> Result<()> {
    if find_tag(&mut File::open(path)?)?.tag.is_none() {
        return Ok(());
    }

    replace_file(path, |src, out| {
        let layout = find_tag(src)?;

        copy_range(src, out, 0, layout.audio_end())?;
        copy_range(src, out, layout.tail_start(), layout.len - layout.tail_start())?;

        Ok(())
    })
}
