// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native FLAC tag writing.
//!
//! The metadata block chain is rewritten: `STREAMINFO` stays first, every block the tag model
//! does not represent (`SEEKTABLE`, `APPLICATION`, `CUESHEET`, ...) is kept, the
//! `VORBIS_COMMENT`, `PICTURE` and `PADDING` blocks are replaced by new ones, and the audio
//! frames are copied unchanged. A leading ID3v2 tag is dropped by default
//! ([`FlacId3::Remove`](crate::FlacId3::Remove)), or all the leading ID3v2 tags are copied as
//! they are in front of the `fLaC` marker ([`FlacId3::Keep`](crate::FlacId3::Keep)). A `SEEKTABLE`
//! block without seek points is dropped if `remove_empty_flac_seektable` is set.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::embedded::flac::write::{
    BLOCK_TYPE_PADDING, BLOCK_TYPE_PICTURE, BLOCK_TYPE_STREAMINFO, BLOCK_TYPE_VORBIS_COMMENT,
    MAX_BLOCK_LEN, block_header, picture_block,
};
use symphonia_metadata::embedded::vorbis::write::{
    CommentOptions, ExistingComments, comment_body_with, parse_existing,
};

use crate::{
    Error, FlacId3, Result, VorbisRatingScale, WriteOptions, copy_range, leading_id3v2_end,
    read_at, read_up_to, replace_file,
};

/// The `SEEKTABLE` block type.
const BLOCK_TYPE_SEEKTABLE: u8 = 3;

/// Gets the Vorbis comment writing options from the write options.
pub(crate) fn comment_options(opts: &WriteOptions) -> CommentOptions {
    CommentOptions {
        rating_email: opts.rating_email.clone(),
        rating_fraction: opts.vorbis_rating_scale == VorbisRatingScale::Fraction,
    }
}

/// The vendor string written when the file does not have one already.
pub(crate) fn default_vendor() -> Vec<u8> {
    concat!("symphonia-tagging ", env!("CARGO_PKG_VERSION")).as_bytes().to_vec()
}

/// The existing comments, or an empty set with the default vendor if there are none.
pub(crate) fn existing_or_default(existing: Option<ExistingComments>) -> ExistingComments {
    existing.unwrap_or_else(|| ExistingComments { vendor: default_vendor(), ..Default::default() })
}

/// The most padding, in bytes, that is kept to leave the audio where it is.
const MAX_REUSED_PADDING: u64 = 64 * 1024;

/// An existing metadata block, located in the source file.
struct Block {
    kind: u8,
    /// The offset of the block body.
    offset: u64,
    len: usize,
}

/// A block of the new chain.
enum Item {
    /// A block copied from the source file.
    Copy(Block),
    /// A block built in memory.
    Mem { kind: u8, data: Vec<u8> },
    /// A padding block of the given body size.
    Padding(usize),
}

impl Item {
    fn kind(&self) -> u8 {
        match self {
            Item::Copy(block) => block.kind,
            Item::Mem { kind, .. } => *kind,
            Item::Padding(_) => BLOCK_TYPE_PADDING,
        }
    }

    fn len(&self) -> usize {
        match self {
            Item::Copy(block) => block.len,
            Item::Mem { data, .. } => data.len(),
            Item::Padding(len) => *len,
        }
    }
}

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| rewrite(src, out, tags, visuals, opts))
}

fn rewrite(
    src: &mut File,
    out: &mut BufWriter<File>,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    let file_len = src.metadata()?.len();

    // Skip the leading ID3v2 tags, if any. They are dropped or kept as is, see `FlacId3`.
    let start = leading_id3v2_end(src)?;

    let had_id3v2 = start > 0;

    src.seek(SeekFrom::Start(start))?;

    let mut marker = [0u8; 4];

    if read_up_to(src, &mut marker)? != 4 || &marker != b"fLaC" {
        return Err(Error::Malformed("missing fLaC marker".into()));
    }

    // Walk the metadata block chain.
    let mut blocks = Vec::new();
    let mut pos = start + 4;

    loop {
        let mut head = [0u8; 4];

        src.seek(SeekFrom::Start(pos))?;

        if read_up_to(src, &mut head)? != 4 {
            return Err(Error::Malformed("metadata block chain ends early".into()));
        }

        let is_last = head[0] & 0x80 != 0;
        let kind = head[0] & 0x7f;
        let len = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;

        if kind == 0x7f {
            return Err(Error::Malformed("invalid metadata block type".into()));
        }

        let offset = pos + 4;

        if offset + len as u64 > file_len {
            return Err(Error::Malformed("metadata block exceeds the file".into()));
        }

        blocks.push(Block { kind, offset, len });
        pos = offset + len as u64;

        if is_last {
            break;
        }
    }

    // The offset of the first audio frame.
    let audio_offset = pos;

    match blocks.first() {
        Some(first) if first.kind == BLOCK_TYPE_STREAMINFO && first.len == 34 => (),
        _ => return Err(Error::Malformed("STREAMINFO is not the first block".into())),
    }

    // The vendor string and the non-tag comments come from the first comment block.
    let existing = match blocks.iter().find(|block| block.kind == BLOCK_TYPE_VORBIS_COMMENT) {
        Some(block) => parse_existing(&read_at(src, block.offset, block.len as u64)?),
        None => None,
    };

    let ExistingComments { vendor, preserved, .. } = existing_or_default(existing);

    // Assemble the new chain: every unmanaged block, then the comment, the pictures and padding.
    let old_len = audio_offset - (start + 4);

    let mut items: Vec<Item> = blocks
        .into_iter()
        .filter(|block| {
            !(matches!(
                block.kind,
                BLOCK_TYPE_VORBIS_COMMENT | BLOCK_TYPE_PICTURE | BLOCK_TYPE_PADDING
            ) || (opts.remove_empty_flac_seektable
                && block.kind == BLOCK_TYPE_SEEKTABLE
                && block.len == 0))
        })
        .map(Item::Copy)
        .collect();

    items.push(Item::Mem {
        kind: BLOCK_TYPE_VORBIS_COMMENT,
        data: comment_body_with(&vendor, tags, &[], &preserved, &comment_options(opts))?,
    });

    for visual in visuals {
        items.push(Item::Mem { kind: BLOCK_TYPE_PICTURE, data: picture_block(visual)? });
    }

    // Reuse the space the old chain occupied so the audio stays where it is, if it can: the new
    // chain must fit with the leftover becoming padding, and the leftover must not be more than
    // `MAX_REUSED_PADDING` or the requested padding (removing a large picture should shrink the
    // file). If not (or if the offset moves anyway because an ID3v2 tag is dropped), leave the
    // requested padding.
    let new_len: u64 = items.iter().map(|item| 4 + item.len() as u64).sum();

    let reusable = old_len
        .checked_sub(new_len + 4)
        .filter(|&padding| padding <= MAX_REUSED_PADDING.max(opts.padding as u64));

    let offset_moves = had_id3v2 && opts.flac_id3 == FlacId3::Remove;
    let padding = if let (false, Some(reusable)) = (offset_moves, reusable) {
        Some(reusable as usize)
    }
    else if opts.padding > 0 {
        Some(opts.padding)
    }
    else {
        None
    };

    if let Some(padding) = padding {
        items.push(Item::Padding(padding.min(MAX_BLOCK_LEN)));
    }

    // Write the new file.
    if opts.flac_id3 == FlacId3::Keep && start > 0 {
        copy_range(src, out, 0, start)?;
    }

    out.write_all(b"fLaC")?;

    let num_items = items.len();

    for (i, item) in items.iter().enumerate() {
        let header = block_header(i + 1 == num_items, item.kind(), item.len())?;
        out.write_all(&header)?;

        match item {
            Item::Copy(block) => copy_range(src, out, block.offset, block.len as u64)?,
            Item::Mem { data, .. } => out.write_all(data)?,
            Item::Padding(len) => write_zeros(out, *len)?,
        }
    }

    // Copy the audio frames, and anything following them, as is.
    src.seek(SeekFrom::Start(audio_offset))?;
    io::copy(src, out)?;

    Ok(())
}

fn write_zeros(out: &mut impl Write, mut len: usize) -> io::Result<()> {
    let zeros = [0u8; 4096];

    while len > 0 {
        let n = len.min(zeros.len());
        out.write_all(&zeros[..n])?;
        len -= n;
    }

    Ok(())
}
