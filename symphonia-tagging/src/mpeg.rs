// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tags of MPEG audio elementary streams (MP1/MP2/MP3) and True Audio (TTA) files.
//!
//! The stream is `[ID3v2 tag(s)] audio [ID3v2 tag] [APEv2 tag] [ID3v1 tag]`:
//!
//! * Every leading ID3v2 tag is replaced by one new tag at the front of the stream. The new tag
//!   is padded to the size of the old tags when it fits in it, so the audio does not move.
//! * An ID3v2 tag that was appended to the stream (found by its `3DI` footer, or by its header
//!   at the position that ends where the APEv2/ID3v1 tags begin) is removed. Its unmanaged
//!   frames are carried over into the new tag.
//! * The ID3v1 tag follows [`WriteOptions::id3v1`](crate::WriteOptions::id3v1): `Preserve`
//!   rewrites an existing tag from the tags (and removes it when there are no tags) but never
//!   adds one, `Write` always writes one unless there are no tags, `Remove` drops it. Whatever
//!   the policy, an enhanced ID3v1 tag (`TAG+`) is replaced by a plain one or removed with it.
//! * An APEv2 tag is not touched, unless
//!   [`WriteOptions::remove_ape_from_mpeg`](crate::WriteOptions::remove_ape_from_mpeg) is set:
//!   then the APEv2 tag (v1 or v2) that ends where the ID3v1 tag(s) begin, or at the end of the
//!   stream, is removed.
//!
//! Raw AAC (ADTS) streams are *not* handled here: they carry an APEv2 tag, see the `ape` module.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::id3v1::write::{ID3V1_ENHANCED_TAG_LEN, ID3V1_TAG_LEN};
use symphonia_metadata::id3v2::write::{Existing, Version};
use symphonia_metadata::{id3v1, id3v2};

use crate::{
    Error, Id3v1Policy, Id3v2Version, Result, WriteOptions, ape, copy_range, read_up_to,
    replace_file,
};

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

    // The new ID3v1 tag, by policy.
    let new_id3v1 = match opts.id3v1 {
        Id3v1Policy::Preserve => has_id3v1 && !tags.is_empty(),
        Id3v1Policy::Write => !tags.is_empty(),
        Id3v1Policy::Remove => false,
    }
    .then(|| id3v1::write::serialize(tags));

    // The APEv2 tag that ends where the ID3v1 tag(s) begin, if it is to be removed.
    let ape_range = if opts.remove_ape_from_mpeg {
        ape::find_tag_before(&mut file, tail_start)?.filter(|range| range.start >= audio_start)
    }
    else {
        None
    };

    // There is nothing to change in a stream without tags that is not given any.
    if new_id3v2.is_empty()
        && audio_start == 0
        && appended.is_none()
        && !has_id3v1
        && new_id3v1.is_none()
        && ape_range.is_none()
    {
        return Ok(());
    }

    drop(file);

    replace_file(path, |src, out| {
        out.write_all(&new_id3v2)?;

        // Copy the audio, skipping the appended ID3v2 tag and the APEv2 tag to remove. The
        // appended tag precedes the APEv2 tag, which ends at `tail_start`.
        let mut pos = audio_start;

        for range in [appended.as_ref(), ape_range.map(|r| r.start..r.end).as_ref()]
            .into_iter()
            .flatten()
        {
            if range.start > pos {
                copy_range(src, out, pos, range.start - pos)?;
            }
            pos = pos.max(range.end);
        }

        copy_range(src, out, pos, tail_start.saturating_sub(pos))?;

        if let Some(id3v1) = &new_id3v1 {
            out.write_all(id3v1)?;
        }

        Ok(())
    })
}
