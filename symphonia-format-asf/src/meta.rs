// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mapping of the ASF Content Description, Extended Content Description, and Metadata objects to
//! Symphonia metadata.
//!
//! The attribute names follow the "WM/" names of the Windows Media Format SDK attribute list
//! (<https://learn.microsoft.com/en-us/windows/win32/wmformat/attribute-list>) and FFmpeg's
//! `ff_asf_metadata_conv` table in `libavformat/asf.c`.

use std::sync::Arc;

use symphonia_core::common::FourCc;
use symphonia_core::errors::Result;
use symphonia_core::meta::{
    MetadataBuilder, MetadataId, MetadataInfo, RawValue, StandardTag, Tag, Visual,
};
use symphonia_metadata::utils::id3v2::get_visual_key_from_picture_type;
use symphonia_metadata::utils::images::{try_get_image_info, visual_from_image_data};

use crate::objects::Cursor;

/// The metadata format of the tags found in an ASF header.
pub const ASF_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"ASF ")),
    short_name: "asf",
    long_name: "ASF Header Metadata",
};

/// The largest attribute (name or value) that is read, in bytes. WM/Picture is exempt.
const MAX_ATTRIBUTE_LEN: usize = 1 << 20;

/// A decoded attribute value.
enum Value<'a> {
    Str(String),
    Bytes(&'a [u8]),
    Bool(bool),
    Uint(u64),
}

/// Decodes a UTF-16LE string, which may be NUL-terminated.
fn utf16_string(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Reads a NUL-terminated UTF-16LE string from the cursor.
fn read_utf16z(c: &mut Cursor<'_>) -> Result<String> {
    let mut units = Vec::new();
    loop {
        let pair = c.take(2)?;
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    Ok(String::from_utf16_lossy(&units))
}

fn tag(key: &str, value: impl Into<RawValue>, std: Option<StandardTag>) -> Tag {
    Tag::new_from_parts(key, value, std)
}

fn arc(s: &str) -> Arc<String> {
    Arc::new(s.to_string())
}

/// Parses `n` or `n/m`.
fn parse_pair(s: &str) -> Option<(u64, Option<u64>)> {
    let (n, m) = match s.split_once('/') {
        Some((n, m)) => (n, m.trim().parse::<u64>().ok()),
        None => (s, None),
    };
    Some((n.trim().parse::<u64>().ok()?, m))
}

/// Maps a string attribute to its standard tags. `name` is the lowercase attribute name.
fn string_std_tags(name: &str, s: &str) -> Vec<StandardTag> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }

    let one = |t| vec![t];

    match name {
        "title" => one(StandardTag::TrackTitle(arc(s))),
        "author" => one(StandardTag::Artist(arc(s))),
        "copyright" => one(StandardTag::Copyright(arc(s))),
        "description" => one(StandardTag::Comment(arc(s))),
        "wm/albumtitle" => one(StandardTag::Album(arc(s))),
        "wm/albumartist" => one(StandardTag::AlbumArtist(arc(s))),
        "wm/genre" => one(StandardTag::Genre(arc(s))),
        "wm/composer" => one(StandardTag::Composer(arc(s))),
        "wm/conductor" => one(StandardTag::Conductor(arc(s))),
        "wm/lyrics" => one(StandardTag::Lyrics(arc(s))),
        "wm/writer" => one(StandardTag::Lyricist(arc(s))),
        "wm/publisher" => one(StandardTag::Label(arc(s))),
        "wm/producer" => one(StandardTag::Producer(arc(s))),
        "wm/modifiedby" => one(StandardTag::Remixer(arc(s))),
        "wm/mood" => one(StandardTag::Mood(arc(s))),
        "wm/language" => one(StandardTag::Language(arc(s))),
        "wm/isrc" => one(StandardTag::IdentIsrc(arc(s))),
        "wm/initialkey" => one(StandardTag::InitialKey(arc(s))),
        "wm/subtitle" => one(StandardTag::TrackSubtitle(arc(s))),
        "wm/setsubtitle" => one(StandardTag::DiscSubtitle(arc(s))),
        "wm/encodedby" => one(StandardTag::EncodedBy(arc(s))),
        "wm/encodingsettings" => one(StandardTag::EncoderSettings(arc(s))),
        "wm/tool" => one(StandardTag::Encoder(arc(s))),
        "wm/originalartist" => one(StandardTag::OriginalArtist(arc(s))),
        "wm/originalalbumtitle" => one(StandardTag::OriginalAlbum(arc(s))),
        "wm/originallyricist" => one(StandardTag::OriginalLyricist(arc(s))),
        "wm/originalfilename" => one(StandardTag::OriginalFile(arc(s))),
        "wm/originalreleaseyear" => {
            s.parse::<u16>().map(StandardTag::OriginalReleaseYear).into_iter().collect()
        }
        "wm/year" => match s.parse::<u16>() {
            Ok(year) => one(StandardTag::RecordingYear(year)),
            Err(_) => one(StandardTag::RecordingDate(arc(s))),
        },
        "wm/beatsperminute" => s.parse::<u64>().map(StandardTag::Bpm).into_iter().collect(),
        "wm/tracknumber" => match parse_pair(s) {
            Some((n, Some(total))) => {
                vec![StandardTag::TrackNumber(n), StandardTag::TrackTotal(total)]
            }
            Some((n, None)) => one(StandardTag::TrackNumber(n)),
            None => Vec::new(),
        },
        "wm/partofset" => match parse_pair(s) {
            Some((n, Some(total))) => {
                vec![StandardTag::DiscNumber(n), StandardTag::DiscTotal(total)]
            }
            Some((n, None)) => one(StandardTag::DiscNumber(n)),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Adds the tags of one attribute to the builder. The first standard tag carries the raw value,
/// further standard tags derived from the same attribute are added without a raw key.
fn add_tags(builder: &mut MetadataBuilder, key: &str, raw: RawValue, mut std: Vec<StandardTag>) {
    let mut std = std.drain(..);
    builder.add_tag(tag(key, raw, std.next()));
    for extra in std {
        builder.add_tag(tag("", "", Some(extra)));
    }
}

/// Adds an attribute to the builder.
fn add_attribute(builder: &mut MetadataBuilder, name: &str, value: Value<'_>) {
    if name.is_empty() {
        return;
    }

    let lower = name.to_ascii_lowercase();

    match value {
        Value::Str(s) => {
            // As in FFmpeg, empty values are not added.
            if s.trim().is_empty() {
                return;
            }
            let std = string_std_tags(&lower, &s);
            add_tags(builder, name, RawValue::from(s), std);
        }
        Value::Bytes(bytes) => {
            if lower == "wm/picture" {
                if let Some(visual) = read_picture(bytes) {
                    builder.add_visual(visual);
                    return;
                }
            }
            builder.add_tag(tag(name, RawValue::from(Box::<[u8]>::from(bytes)), None));
        }
        Value::Bool(b) => {
            builder.add_tag(tag(name, RawValue::Boolean(b), None));
        }
        Value::Uint(v) => {
            // WM/Track is the zero-based track number, WM/TrackNumber the one-based track number.
            let std = if lower == "wm/track" {
                vec![StandardTag::TrackNumber(v.saturating_add(1))]
            }
            else {
                string_std_tags(&lower, &v.to_string())
            };
            add_tags(builder, name, RawValue::UnsignedInt(v), std);
        }
    }
}

/// Decodes the value of an attribute of the given data type (spec 3.11, 4.7). A BOOL is true if any
/// of its bytes is non-zero, which covers both its 16-bit and 32-bit representations.
fn decode_value(data_type: u16, data: &[u8]) -> Option<Value<'_>> {
    match data_type {
        // Unicode string.
        0 => Some(Value::Str(utf16_string(data))),
        // Byte array.
        1 => Some(Value::Bytes(data)),
        // BOOL.
        2 => Some(Value::Bool(data.iter().any(|&b| b != 0))),
        // DWORD.
        3 => {
            let b = data.get(..4)?;
            Some(Value::Uint(u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))))
        }
        // QWORD.
        4 => {
            let b = data.get(..8)?;
            Some(Value::Uint(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])))
        }
        // WORD.
        5 => {
            let b = data.get(..2)?;
            Some(Value::Uint(u64::from(u16::from_le_bytes([b[0], b[1]]))))
        }
        // GUID and unknown types are not mapped.
        _ => None,
    }
}

/// Reads a Content Description Object body (spec 3.10): the lengths of the title, author,
/// copyright, description and rating strings followed by the UTF-16LE strings.
pub fn read_content_description(body: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    let mut c = Cursor::new(body);

    let mut lens = [0usize; 5];
    for len in &mut lens {
        *len = usize::from(c.u16()?);
    }

    const NAMES: [&str; 5] = ["Title", "Author", "Copyright", "Description", "Rating"];

    for (name, len) in NAMES.iter().zip(lens) {
        let data = c.take(len)?;
        add_attribute(builder, name, Value::Str(utf16_string(data)));
    }

    Ok(())
}

/// Reads an Extended Content Description Object body (spec 3.11).
pub fn read_extended_content_description(body: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    let mut c = Cursor::new(body);
    let count = c.u16()?;

    for _ in 0..count {
        let name_len = usize::from(c.u16()?);
        let name = utf16_string(c.take(name_len)?);
        let data_type = c.u16()?;
        let data_len = usize::from(c.u16()?);
        let data = c.take(data_len)?;

        if let Some(value) = decode_value(data_type, data) {
            add_attribute(builder, &name, value);
        }
    }

    Ok(())
}

/// Reads a Metadata Object or Metadata Library Object body (spec 4.7, 4.8). Only attributes that
/// apply to the whole file (stream number 0) are kept.
///
/// Both objects start with a record count, followed by records of: Language List Index or
/// Reserved (2), Stream Number (2), Name Length (2), Data Type (2), Data Length (4), Name, Data.
pub fn read_metadata_object(body: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    let mut c = Cursor::new(body);
    let count = c.u16()?;

    for _ in 0..count {
        c.skip(2)?;
        let stream = c.u16()?;
        let name_len = usize::from(c.u16()?);
        let data_type = c.u16()?;
        let data_len = c.u32()? as usize;
        let name = utf16_string(c.take(name_len)?);
        let data = c.take(data_len)?;

        if stream != 0 || name.len() > MAX_ATTRIBUTE_LEN {
            continue;
        }

        if let Some(value) = decode_value(data_type, data) {
            add_attribute(builder, &name, value);
        }
    }

    Ok(())
}

/// Reads a `WM/Picture` attribute: Picture Type (1), Picture Data Size (4), MIME type (UTF-16LE,
/// NUL-terminated), Description (UTF-16LE, NUL-terminated), Picture Data.
fn read_picture(data: &[u8]) -> Option<Visual> {
    let mut c = Cursor::new(data);
    let picture_type = c.u8().ok()?;
    let size = c.u32().ok()? as usize;
    let mime = read_utf16z(&mut c).ok()?;
    let desc = read_utf16z(&mut c).ok()?;

    // Trust the declared size only if it fits.
    let image = c.take(size.min(c.remaining())).ok()?;
    if image.is_empty() {
        return None;
    }

    let mut tags = Vec::new();
    if !desc.is_empty() {
        tags.push(Tag::new_from_parts("", "", Some(StandardTag::Description(Arc::new(desc)))));
    }

    let image = Box::<[u8]>::from(image);
    let info = try_get_image_info(&image);
    let media_type = (!mime.is_empty()).then_some(mime);
    let usage = get_visual_key_from_picture_type(u32::from(picture_type));

    Some(visual_from_image_data(image, info, media_type, None, usage, tags))
}
