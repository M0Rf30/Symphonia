// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading the tags of the tag-only containers Symphonia has no demuxer for.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use symphonia_core::formats::probe::ProbeableMetadata;
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::{MetadataOptions, MetadataReader, Tag, Visual};
use symphonia_metadata::ape::ApeReader;
use symphonia_metadata::id3v1::Id3v1Reader;
use symphonia_metadata::id3v1::write::ID3V1_TAG_LEN;
use symphonia_metadata::id3v2;
use symphonia_metadata::id3v2::Id3v2Reader;

use crate::{Container, Error, Result, ape, read_up_to, sniff};

/// The tags and visuals [`read_tags`] found in a file.
#[derive(Debug, Clone, Default)]
pub struct ReadTags {
    /// The tags, from the highest to the lowest priority: ID3v2, then APEv2, then ID3v1.
    pub tags: Vec<Tag>,
    /// The visuals (cover art), in the same order as the tags.
    pub visuals: Vec<Visual>,
}

fn malformed(err: impl std::fmt::Display) -> Error {
    Error::Malformed(err.to_string())
}

/// Reads the tags of a file in a container that Symphonia has no demuxer for, so they cannot be
/// read through the Symphonia probe:
///
/// * [`Container::Ac3`], [`Container::Tak`], [`Container::OptimFrog`]: the APEv2 tag at the end of
///   the file.
/// * [`Container::Aac`] (ADTS): the APEv2 tag at the end of the file. Symphonia reads the audio of
///   these streams, but its probe does not necessarily find that tag.
/// * [`Container::Tta`]: the ID3v2 tag(s) at the front, the APEv2 tag at the end, and the ID3v1
///   tag at the very end.
///
/// The tags are merged with the lowest priority last, as the probe orders the metadata of a file:
/// ID3v2, then APEv2, then ID3v1. A file without tags yields empty lists.
///
/// # Errors
///
/// [`Error::Unsupported`] for every other container; those are read through the Symphonia probe.
pub fn read_tags(path: &Path) -> Result<ReadTags> {
    let container = sniff(path)?;

    let (id3v2, ape, id3v1) = match container {
        Container::Ac3 | Container::Tak | Container::OptimFrog | Container::Aac => {
            (false, true, false)
        }
        Container::Tta => (true, true, true),
        _ => return Err(Error::Unsupported(path.display().to_string())),
    };

    let mut out = ReadTags::default();

    if id3v2 {
        read_leading_id3v2(path, &mut out)?;
    }

    if ape {
        read_ape(path, &mut out)?;
    }

    if id3v1 {
        read_id3v1(path, &mut out)?;
    }

    Ok(out)
}

fn read_leading_id3v2(path: &Path, out: &mut ReadTags) -> Result<()> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut pos = 0u64;

    loop {
        let mut header = [0u8; 10];
        file.seek(SeekFrom::Start(pos))?;

        if read_up_to(&mut file, &mut header)? < header.len() {
            break;
        }

        let Some(tag_len) = id3v2::write::tag_len(&header)
        else {
            break;
        };

        if pos + tag_len as u64 > len {
            return Err(Error::Malformed(format!(
                "the ID3v2 tag at {pos} extends past the end of the file"
            )));
        }

        file.seek(SeekFrom::Start(pos))?;

        let mss = MediaSourceStream::new(Box::new(file.try_clone()?), Default::default());
        let rev = Id3v2Reader::try_new(mss, MetadataOptions::default())
            .and_then(|mut reader| reader.read_all())
            .map_err(malformed)?
            .revision;

        out.tags.extend(rev.media.tags);
        out.visuals.extend(rev.media.visuals);

        pos += tag_len as u64;
    }

    Ok(())
}

fn read_ape(path: &Path, out: &mut ReadTags) -> Result<()> {
    let mut file = File::open(path)?;

    let Some(range) = ape::find_tag(&mut file)?.tag
    else {
        return Ok(());
    };

    // The reader accepts a header or a footer, and reads exactly the tag's items.
    file.seek(SeekFrom::Start(range.start))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let rev = ApeReader::try_probe_new(mss, MetadataOptions::default())
        .and_then(|mut reader| reader.read_all())
        .map_err(malformed)?
        .revision;

    out.tags.extend(rev.media.tags);
    out.visuals.extend(rev.media.visuals);

    Ok(())
}

fn read_id3v1(path: &Path, out: &mut ReadTags) -> Result<()> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    let Some(start) = len.checked_sub(ID3V1_TAG_LEN as u64)
    else {
        return Ok(());
    };

    let mut marker = [0u8; 3];
    file.seek(SeekFrom::Start(start))?;

    if read_up_to(&mut file, &mut marker)? < 3 || &marker != b"TAG" {
        return Ok(());
    }

    // The reader expects to be positioned at the tag, and looks for an enhanced tag before it. The
    // stream's position is counted from where it was created, so seek to the tag through it.
    let mut mss = MediaSourceStream::new(Box::new(file), Default::default());
    mss.seek(SeekFrom::Start(start))?;

    let rev = Id3v1Reader::try_new(mss, MetadataOptions::default())
        .and_then(|mut reader| reader.read_all())
        .map_err(malformed)?
        .revision;

    out.tags.extend(rev.media.tags);
    out.visuals.extend(rev.media.visuals);

    Ok(())
}
