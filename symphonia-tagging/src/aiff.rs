// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tag writing for AIFF and AIFC files.
//!
//! The reader takes tags from an `ID3 ` chunk (ID3v2) and from the text chunks `NAME` (track
//! title), `AUTH` (artist), `(c) ` (copyright) and `ANNO` (comment). A write rebuilds both from
//! the given tags: the `ID3 ` chunk is replaced, added, or removed when there is nothing to write,
//! and the text chunks are rewritten. A tag of the same kind beyond the first is only kept in the
//! `ID3 ` chunk, except for `ANNO`, which may repeat. Every other chunk (`COMM`, `SSND`, `MARK`,
//! `COMT`, `APPL`, ...) is copied byte for byte, in order.

use std::path::Path;

use symphonia_core::meta::{StandardTag, Tag, Visual};

use crate::riff::{Item, build_id3, emit, make_chunk, scan};
use crate::{Error, Result, WriteOptions, replace_file};

/// The text chunk for a standard tag, and whether the chunk may repeat.
fn text_chunk(std: &StandardTag) -> Option<([u8; 4], &str, bool)> {
    match std {
        StandardTag::TrackTitle(v) => Some((*b"NAME", v, false)),
        StandardTag::Artist(v) => Some((*b"AUTH", v, false)),
        StandardTag::Copyright(v) => Some((*b"(c) ", v, false)),
        StandardTag::Comment(v) => Some((*b"ANNO", v, true)),
        _ => None,
    }
}

fn is_text_chunk(id: &[u8; 4]) -> bool {
    matches!(id, b"NAME" | b"AUTH" | b"(c) " | b"ANNO")
}

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| {
        let (magic, form, scan) = scan(src, true)?;

        if &magic != b"FORM" || (&form != b"AIFF" && &form != b"AIFC") {
            return Err(Error::Unsupported("not an AIFF/AIFC file".into()));
        }

        // The new text chunks.
        let mut text_chunks = Vec::new();
        let mut seen: Vec<[u8; 4]> = Vec::new();

        for tag in tags {
            let Some((id, value, repeats)) = tag.std.as_ref().and_then(text_chunk)
            else {
                continue;
            };

            if !repeats && seen.contains(&id) {
                continue;
            }

            let mut payload = value.as_bytes().to_vec();
            payload.retain(|&b| b != 0);

            if payload.is_empty() {
                continue;
            }

            seen.push(id);
            text_chunks.push(Item::New(make_chunk(id, &payload, true)?));
        }

        let id3 = build_id3(src, &scan, tags, visuals, opts)?;
        let id3_chunk = if id3.is_empty() { None } else { Some(make_chunk(*b"ID3 ", &id3, true)?) };

        // Replace the first text chunk and the first ID3 chunk in place, drop the others, and add
        // the new chunks after the last chunk if the file had none.
        let mut text_chunks = Some(text_chunks);
        let mut id3_chunk = id3_chunk;
        let mut seen_id3 = false;
        let mut items = Vec::with_capacity(scan.chunks.len() + 2);

        for (i, chunk) in scan.chunks.iter().enumerate() {
            if is_text_chunk(&chunk.id) {
                items.extend(text_chunks.take().into_iter().flatten());
            }
            else if chunk.is_id3() {
                if !std::mem::replace(&mut seen_id3, true) {
                    items.extend(id3_chunk.take().map(Item::New));
                }
            }
            else {
                items.push(Item::Copy(i));
            }
        }

        items.extend(text_chunks.into_iter().flatten());
        items.extend(id3_chunk.map(Item::New));

        emit(src, out, &scan, magic, form, true, &items)
    })
}
