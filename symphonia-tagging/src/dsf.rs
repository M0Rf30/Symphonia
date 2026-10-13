// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tag writing for DSF files.
//!
//! A DSF file is a 28-byte `DSD ` header (chunk size, total file size, and the offset of the
//! metadata), a 52-byte `fmt ` chunk, and a `data` chunk, followed by an ID3v2 tag that runs from
//! the metadata offset to the end of the file. A write replaces that tag (keeping its unmanaged
//! frames), and updates the total file size and the metadata offset (0 when there is no tag).
//! Everything before the tag is copied byte for byte.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};

use crate::riff::serialize_id3;
use crate::{Error, Result, WriteOptions, copy_range, read_at, read_up_to, replace_file};

/// The size of the `DSD ` header and the `fmt ` chunk.
const HEADERS_LEN: u64 = 28 + 52;

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| {
        let file_len = src.metadata()?.len();

        // The `DSD ` header, the `fmt ` chunk header, and the `data` chunk header.
        let mut head = [0u8; 92];
        src.seek(SeekFrom::Start(0))?;
        if read_up_to(src, &mut head)? < head.len() {
            return Err(Error::Malformed("file is shorter than the DSF headers".into()));
        }

        if &head[0..4] != b"DSD " || le_u64(&head[4..12]) != 28 {
            return Err(Error::Malformed("bad DSF header".into()));
        }
        if &head[28..32] != b"fmt " || le_u64(&head[32..40]) != 52 {
            return Err(Error::Malformed("bad DSF fmt chunk".into()));
        }
        if &head[80..84] != b"data" {
            return Err(Error::Malformed("bad DSF data chunk".into()));
        }

        // The data chunk size includes its 12-byte header.
        let data_end = le_u64(&head[84..92])
            .checked_add(HEADERS_LEN)
            .filter(|&end| end <= file_len)
            .ok_or_else(|| {
                Error::Malformed("DSF data chunk extends past the end of the file".into())
            })?;

        // The tag runs from the metadata pointer to the end of the file. Without a pointer, no tag
        // exists, and a new one goes at the end of the file.
        let pointer = le_u64(&head[20..28]);
        let tag_start = if pointer == 0 { file_len } else { pointer };

        if tag_start < data_end || tag_start > file_len {
            return Err(Error::Malformed("DSF metadata pointer is out of range".into()));
        }

        let old_tag = if tag_start < file_len {
            vec![read_at(src, tag_start, file_len - tag_start)?]
        }
        else {
            Vec::new()
        };

        let id3 = serialize_id3(&old_tag, tags, visuals, opts);

        let new_len = tag_start + id3.len() as u64;
        let new_pointer = if id3.is_empty() { 0 } else { tag_start };

        // `head` holds the first 92 bytes: patch the total size and the metadata pointer in the
        // first 28 of them.
        head[12..20].copy_from_slice(&new_len.to_le_bytes());
        head[20..28].copy_from_slice(&new_pointer.to_le_bytes());

        out.write_all(&head[..28])?;
        copy_range(src, out, 28, tag_start - 28)?;
        out.write_all(&id3)?;

        Ok(())
    })
}
