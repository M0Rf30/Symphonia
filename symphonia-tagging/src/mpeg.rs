// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tags of MPEG audio elementary streams (MP1/MP2/MP3, ADTS AAC).
//!
//! The stream is `[ID3v2 tag(s)] audio [ID3v2 tag] [APEv2 tag] [ID3v1 tag]`:
//!
//! * Every leading ID3v2 tag is replaced by one new tag at the front of the stream. The new tag
//!   is padded to the size of the old tags when it fits in it, so the audio does not move.
//! * An ID3v2 tag that was appended to the stream (found by its `3DI` footer, or by its header
//!   at the position that ends where the APEv2/ID3v1 tags begin) is removed. Its unmanaged
//!   frames are carried over into the new tag.
//! * An ID3v1 tag is rewritten from the tags. One is never added, and when there are no tags it
//!   is removed. An enhanced ID3v1 tag (`TAG+`) is replaced by a plain one.
//! * An APEv2 tag is not touched.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::id3v1::write::{ID3V1_ENHANCED_TAG_LEN, ID3V1_TAG_LEN};
use symphonia_metadata::id3v2::write::{Existing, Version};
use symphonia_metadata::{id3v1, id3v2};

use crate::{Error, Id3v2Version, Result, WriteOptions, copy_range, read_up_to, replace_file};

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    let mut existing = Existing::new();

    // The leading ID3v2 tags.
    let mut audio_start = 0u64;

    loop {
        let mut header = [0u8; 10];
        file.seek(SeekFrom::Start(audio_start))?;

        if read_up_to(&mut file, &mut header)? < header.len() {
            break;
        }

        let Some(tag_len) = id3v2::write::tag_len(&header)
        else {
            break;
        };

        let tag_len = tag_len as u64;

        if audio_start + tag_len > len {
            return Err(Error::Malformed(format!(
                "the ID3v2 tag at {audio_start} extends past the end of the file"
            )));
        }

        let mut tag = vec![0u8; tag_len as usize];
        file.seek(SeekFrom::Start(audio_start))?;
        file.read_exact(&mut tag)?;

        existing.parse_tag(&tag);
        audio_start += tag_len;
    }

    // The ID3v1 tag, and the enhanced ID3v1 tag that may precede it.
    let mut tail_start = len;
    let mut has_id3v1 = false;

    if len >= audio_start + ID3V1_TAG_LEN as u64 {
        let tag_start = len - ID3V1_TAG_LEN as u64;
        let mut marker = [0u8; 4];

        file.seek(SeekFrom::Start(tag_start))?;
        file.read_exact(&mut marker[..3])?;

        if &marker[..3] == b"TAG" {
            has_id3v1 = true;
            tail_start = tag_start;

            if let Some(enhanced_start) =
                tag_start.checked_sub(ID3V1_ENHANCED_TAG_LEN as u64).filter(|&s| s >= audio_start)
            {
                file.seek(SeekFrom::Start(enhanced_start))?;
                file.read_exact(&mut marker)?;

                if &marker == b"TAG+" {
                    tail_start = enhanced_start;
                }
            }
        }
    }

    // An appended ID3v2 tag.
    let appended = {
        let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
        let mut mss = mss;
        id3v2::locate_trailing_id3v2(&mut mss, len)
            .map_err(|err| Error::Malformed(err.to_string()))?
            .filter(|range| range.start >= audio_start && range.end <= tail_start)
    };

    if let Some(range) = &appended {
        let mut tag = vec![0u8; (range.end - range.start) as usize];
        file.seek(SeekFrom::Start(range.start))?;
        file.read_exact(&mut tag)?;
        existing.parse_tag(&tag);
    }

    // Build the new ID3v2 tag.
    let version = match opts.id3v2_version {
        Id3v2Version::V3 => Version::V3,
        Id3v2Version::V4 => Version::V4,
        Id3v2Version::Preserve if existing.version() == Some(3) => Version::V3,
        Id3v2Version::Preserve => Version::V4,
    };

    let id3v2_opts =
        opts.id3v2_options(version, opts.padding, (audio_start > 0).then_some(audio_start as usize));

    let new_id3v2 = id3v2::write::serialize(tags, visuals, &existing, &id3v2_opts);

    // The ID3v1 tag is rewritten, never added.
    let new_id3v1 = (has_id3v1 && !tags.is_empty()).then(|| id3v1::write::serialize(tags));

    // There is nothing to change in a stream without tags that is not given any.
    if new_id3v2.is_empty() && audio_start == 0 && appended.is_none() && !has_id3v1 {
        return Ok(());
    }

    drop(file);

    replace_file(path, |src, out| {
        out.write_all(&new_id3v2)?;

        match &appended {
            Some(range) => {
                // Skip the appended tag.
                copy_range(src, out, audio_start, range.start - audio_start)?;
                copy_range(src, out, range.end, tail_start - range.end)?;
            }
            None => copy_range(src, out, audio_start, tail_start - audio_start)?,
        }

        if let Some(id3v1) = &new_id3v1 {
            out.write_all(id3v1)?;
        }

        Ok(())
    })
}
