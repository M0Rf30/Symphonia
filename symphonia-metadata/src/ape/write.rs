// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An APEv2 tag serializer.
//!
//! [`serialize`] is the reverse of [`ApeReader`](super::ApeReader): a [`Tag`] with a standard tag
//! is written to the native item the reader maps to that standard tag (using the item name that
//! MusicBrainz Picard writes where the reader accepts several), a [`Tag`] without one is written
//! under its raw key, and a [`Visual`] is written as a `Cover Art (<usage>)` binary item whose
//! value is `<file name>\0<image data>`.
//!
//! The output is a complete APEv2 tag: a header, the items sorted by size, and a footer.

use std::collections::HashMap;
use std::fmt::Display;

use symphonia_core::meta::{RawValue, StandardTag, StandardVisualKey, Tag, Visual};

use crate::utils::images::try_get_image_info;

use super::COVER_ART_KEYS;

/// The size of an APE tag header or footer.
pub const APE_HEADER_SIZE: usize = 32;

/// The identifier that starts every APE tag header and footer.
pub const APE_PREAMBLE: &[u8; 8] = b"APETAGEX";

const APE_VERSION_2: u32 = 2000;

pub use crate::utils::ape::{FLAG_HAS_HEADER, FLAG_IS_HEADER};

/// Item type (bits 1..=2 of the item flags): UTF-8 text.
const ITEM_TEXT: u32 = 0 << 1;
/// Item type: binary data.
const ITEM_BINARY: u32 = 1 << 1;
/// Item type: UTF-8 locator (external link).
const ITEM_LOCATOR: u32 = 2 << 1;

/// Keys that may not be used for an item.
const RESERVED_KEYS: [&str; 4] = ["id3", "tag", "oggs", "mp+"];

/// The type of the value of an item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Text,
    Locator,
    Binary,
}

struct Item {
    key: String,
    kind: ItemKind,
    value: Vec<u8>,
}

impl Item {
    /// The number of bytes this item occupies in the tag.
    fn size(&self) -> usize {
        8 + self.key.len() + 1 + self.value.len()
    }
}

/// Serializes `tags` and `visuals` into a complete APEv2 tag (header, items, footer).
///
/// An item whose key (compared case-insensitively) is in `locator_keys` is written as a
/// locator (external link) instead of text, which keeps the type of an item that was read as a
/// locator. [`locator_keys`] lists those keys for an existing tag.
///
/// Returns an empty buffer when there is nothing to write.
pub fn serialize(tags: &[Tag], visuals: &[Visual], locator_keys: &[String]) -> Vec<u8> {
    let mut items = collect_items(tags, visuals, locator_keys);

    if items.is_empty() {
        return Vec::new();
    }

    // The specification recommends sorting items by size, smallest first. The sort is stable so
    // items of equal size keep their order.
    items.sort_by_key(Item::size);

    let body_len: usize = items.iter().map(Item::size).sum();
    let size = (body_len + APE_HEADER_SIZE) as u32;
    let count = items.len() as u32;

    let mut out = Vec::with_capacity(body_len + 2 * APE_HEADER_SIZE);

    write_header(&mut out, size, count, FLAG_HAS_HEADER | FLAG_IS_HEADER);

    for item in &items {
        let kind = match item.kind {
            ItemKind::Text => ITEM_TEXT,
            ItemKind::Locator => ITEM_LOCATOR,
            ItemKind::Binary => ITEM_BINARY,
        };

        out.extend_from_slice(&(item.value.len() as u32).to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(item.key.as_bytes());
        out.push(0);
        out.extend_from_slice(&item.value);
    }

    write_header(&mut out, size, count, FLAG_HAS_HEADER);

    out
}

/// Lists the keys of the locator items in `tag`, a complete APE tag including its header (if any)
/// and footer, so that [`serialize`] can keep them locators. Returns an empty list if `tag` is
/// malformed or is an APEv1 tag (which only has text items).
pub fn locator_keys(tag: &[u8]) -> Vec<String> {
    let mut keys = Vec::new();
    let _ = collect_locator_keys(tag, &mut keys);
    keys
}

/// Collects the keys of the locator items of `tag` into `keys`. Returns `None` when the tag ends
/// or is malformed before all items were read; the keys collected up to then are kept.
fn collect_locator_keys(tag: &[u8], keys: &mut Vec<String>) -> Option<()> {
    let footer = &tag[tag.len().checked_sub(APE_HEADER_SIZE)?..];

    if &footer[..8] != APE_PREAMBLE || read_u32(&footer[8..])? != APE_VERSION_2 {
        return None;
    }

    let count = read_u32(&footer[16..])?;
    let has_header = read_u32(&footer[20..])? & FLAG_HAS_HEADER != 0;

    let start = if has_header { APE_HEADER_SIZE } else { 0 };
    let mut items = &tag[start.min(tag.len() - APE_HEADER_SIZE)..tag.len() - APE_HEADER_SIZE];

    for _ in 0..count {
        let len = read_u32(items)? as usize;
        let flags = read_u32(items.get(4..)?)?;

        let key_len = items.get(8..)?.iter().position(|&b| b == 0)?;
        let next = items.get(8 + key_len + 1..)?.get(len..)?;

        if flags & (3 << 1) == ITEM_LOCATOR {
            if let Ok(key) = std::str::from_utf8(&items[8..8 + key_len]) {
                keys.push(key.to_string());
            }
        }

        items = next;
    }

    Some(())
}

fn read_u32(buf: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(..4)?.try_into().ok()?))
}

fn write_header(out: &mut Vec<u8>, size: u32, count: u32, flags: u32) {
    out.extend_from_slice(APE_PREAMBLE);
    out.extend_from_slice(&APE_VERSION_2.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
}

/// The text items of a single key, in the order their values were added.
struct Field {
    key: String,
    values: Vec<String>,
}

/// Accumulates text values by key (case-insensitively), so repeated values become one
/// NUL-separated multi-value item.
#[derive(Default)]
struct Fields {
    fields: Vec<Field>,
    index: HashMap<String, usize>,
}

impl Fields {
    fn add(&mut self, key: &str, value: String) {
        if value.is_empty() {
            return;
        }

        let lower = key.to_ascii_lowercase();

        match self.index.get(&lower) {
            Some(&i) => {
                // The recording and track ids share an item and may both be given.
                if !self.fields[i].values.contains(&value) {
                    self.fields[i].values.push(value);
                }
            }
            None => {
                self.index.insert(lower, self.fields.len());
                self.fields.push(Field { key: key.to_string(), values: vec![value] });
            }
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.index.contains_key(&key.to_ascii_lowercase())
    }
}

/// Returns `true` if `key` may be used as the key of an APE item: 2 to 255 printable ASCII
/// characters, and not one of the reserved keys.
fn is_valid_key(key: &str) -> bool {
    (2..=255).contains(&key.len())
        && key.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && !RESERVED_KEYS.contains(&key.to_ascii_lowercase().as_str())
}

fn is_cover_key(key: &str) -> bool {
    key.len() >= 9 && key[..9].eq_ignore_ascii_case("cover art")
}

/// Formats a "NUM/TOTAL" or "NUM" value.
fn m_of_n(num: u64, total: Option<u64>) -> String {
    match total {
        Some(total) => format!("{num}/{total}"),
        None => num.to_string(),
    }
}

fn collect_items(tags: &[Tag], visuals: &[Visual], locator_keys: &[String]) -> Vec<Item> {
    let mut fields = Fields::default();
    let mut binaries = Vec::new();

    let mut track = (None, None);
    let mut disc = (None, None);

    for tag in tags {
        match &tag.std {
            Some(StandardTag::TrackNumber(n)) => {
                track.0.get_or_insert(*n);
            }
            Some(StandardTag::TrackTotal(n)) => {
                track.1.get_or_insert(*n);
            }
            Some(StandardTag::DiscNumber(n)) => {
                disc.0.get_or_insert(*n);
            }
            Some(StandardTag::DiscTotal(n)) => {
                disc.1.get_or_insert(*n);
            }
            Some(std) => match native_field(std) {
                Some((key, value)) => fields.add(key, value),
                None => add_raw(tag, &mut fields, &mut binaries),
            },
            None => add_raw(tag, &mut fields, &mut binaries),
        }
    }

    // A year read from a different format has no native item of its own; it is only written if
    // there is no full release date.
    if !fields.contains("year") {
        for tag in tags {
            if let Some(StandardTag::ReleaseYear(year)) = &tag.std {
                fields.add("Year", year.to_string());
            }
        }
    }

    // The track and disc numbers and totals are combined into a single "NUM/TOTAL" item. A total
    // without a number has no such form, and is written as its own item.
    for (key, total_key, (num, total)) in
        [("Track", "TrackTotal", track), ("Disc", "DiscTotal", disc)]
    {
        match num {
            Some(num) => fields.add(key, m_of_n(num, total)),
            None => {
                if let Some(total) = total {
                    fields.add(total_key, total.to_string());
                }
            }
        }
    }

    let mut items: Vec<Item> = Vec::new();

    for field in fields.fields {
        let kind = if locator_keys.iter().any(|k| k.eq_ignore_ascii_case(&field.key)) {
            ItemKind::Locator
        }
        else {
            ItemKind::Text
        };

        items.push(Item { key: field.key, kind, value: field.values.join("\0").into_bytes() });
    }

    items.extend(binaries);

    for visual in visuals {
        items.push(visual_item(visual));
    }

    items
}

/// Adds a tag that has no native mapping under its raw key.
fn add_raw(tag: &Tag, fields: &mut Fields, binaries: &mut Vec<Item>) {
    let key = tag.raw.key.as_str();

    if !is_valid_key(key) {
        log::warn!("ape: skipping a tag with an invalid key ({key:?})");
        return;
    }

    match &tag.raw.value {
        RawValue::Binary(data) => {
            // Cover art is written from the visuals only.
            if !is_cover_key(key) && !data.is_empty() {
                binaries.push(Item {
                    key: key.to_string(),
                    kind: ItemKind::Binary,
                    value: data.to_vec(),
                });
            }
        }
        RawValue::String(value) => fields.add(key, value.to_string()),
        RawValue::StringList(values) => {
            for value in values.iter() {
                fields.add(key, value.clone());
            }
        }
        RawValue::Boolean(v) => fields.add(key, if *v { "1" } else { "0" }.to_string()),
        RawValue::Float(v) => fields.add(key, v.to_string()),
        RawValue::SignedInt(v) => fields.add(key, v.to_string()),
        RawValue::UnsignedInt(v) => fields.add(key, v.to_string()),
        // A flag has no value, and an item without a value is not representable.
        _ => (),
    }
}

/// The item key for a visual.
fn cover_key(usage: Option<StandardVisualKey>) -> &'static str {
    let usage = usage.unwrap_or(StandardVisualKey::Other);

    COVER_ART_KEYS
        .iter()
        .find(|(_, key_usage)| *key_usage == usage)
        .map_or(COVER_ART_KEYS[0].0, |(key, _)| key)
}

fn extension_for(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Creates the binary item of a visual: `<file name>\0<data>`.
fn visual_item(visual: &Visual) -> Item {
    // The file name is the visual's original file name if it has one, otherwise one is made up
    // from the media type (given, or detected from the image data).
    let name = visual.tags.iter().find_map(|tag| match (&tag.std, &tag.raw.value) {
        (Some(StandardTag::OriginalFile(name)), _) => Some(name.to_string()),
        (None, RawValue::String(name)) if tag.raw.key.eq_ignore_ascii_case("file") => {
            Some(name.to_string())
        }
        _ => None,
    });

    let name = name.filter(|name| !name.is_empty() && !name.contains('\0')).unwrap_or_else(|| {
        let media_type = match &visual.media_type {
            Some(media_type) => Some(media_type.clone()),
            None => try_get_image_info(&visual.data).map(|info| info.media_type),
        };

        match media_type {
            Some(media_type) => format!("cover.{}", extension_for(&media_type)),
            None => "cover".to_string(),
        }
    });

    let mut value = Vec::with_capacity(name.len() + 1 + visual.data.len());
    value.extend_from_slice(name.as_bytes());
    value.push(0);
    value.extend_from_slice(&visual.data);

    Item { key: cover_key(visual.usage).to_string(), kind: ItemKind::Binary, value }
}

/// Gets the native item key and the value to write for a standard tag, if the reader maps an
/// item to it.
///
/// Track and disc numbers and totals are not handled here, they are combined by the caller.
fn native_field(std: &StandardTag) -> Option<(&'static str, String)> {
    fn s(v: &impl Display) -> String {
        v.to_string()
    }

    let field = match std {
        StandardTag::AccurateRipCount(v) => ("AccurateRipCount", s(v)),
        StandardTag::AccurateRipCountAllOffsets(v) => ("AccurateRipCountAllOffsets", s(v)),
        StandardTag::AccurateRipCountWithOffset(v) => ("AccurateRipCountWithOffset", s(v)),
        StandardTag::AccurateRipCrc(v) => ("AccurateRipCrc", s(v)),
        StandardTag::AccurateRipDiscId(v) => ("AccurateRipDiscId", s(v)),
        StandardTag::AccurateRipId(v) => ("AccurateRipId", s(v)),
        StandardTag::AccurateRipOffset(v) => ("AccurateRipOffset", s(v)),
        StandardTag::AccurateRipResult(v) => ("AccurateRipResult", s(v)),
        StandardTag::AccurateRipTotal(v) => ("AccurateRipTotal", s(v)),
        StandardTag::AcoustIdFingerprint(v) => ("ACOUSTID_FINGERPRINT", s(v)),
        StandardTag::AcoustIdId(v) => ("ACOUSTID_ID", s(v)),
        StandardTag::Album(v) => ("Album", s(v)),
        StandardTag::AlbumArtist(v) => ("Album Artist", s(v)),
        StandardTag::Arranger(v) => ("Arranger", s(v)),
        StandardTag::Artist(v) => ("Artist", s(v)),
        StandardTag::Bpm(v) => ("BPM", s(v)),
        StandardTag::Comment(v) => ("Comment", s(v)),
        StandardTag::CompilationFlag(v) => ("Compilation", if *v { "1" } else { "0" }.to_string()),
        StandardTag::Composer(v) => ("Composer", s(v)),
        StandardTag::Conductor(v) => ("Conductor", s(v)),
        StandardTag::Copyright(v) => ("Copyright", s(v)),
        StandardTag::EncodedBy(v) => ("EncodedBy", s(v)),
        StandardTag::Encoder(v) => ("Encoder", s(v)),
        StandardTag::EncoderSettings(v) => ("Encoder Settings", s(v)),
        StandardTag::Engineer(v) => ("Engineer", s(v)),
        StandardTag::Genre(v) => ("Genre", s(v)),
        StandardTag::IdentAsin(v) => ("ASIN", s(v)),
        StandardTag::IdentCatalogNumber(v) => ("CatalogNumber", s(v)),
        StandardTag::IdentEanUpn(v) => ("EAN/UPC", s(v)),
        StandardTag::IdentIsbn(v) => ("ISBN", s(v)),
        StandardTag::IdentIsrc(v) => ("ISRC", s(v)),
        StandardTag::Label(v) => ("Label", s(v)),
        StandardTag::LabelCode(v) => ("LabelCode", s(v)),
        StandardTag::Language(v) => ("Language", s(v)),
        StandardTag::Lyricist(v) => ("Lyricist", s(v)),
        StandardTag::Lyrics(v) => ("Lyrics", s(v)),
        StandardTag::MediaFormat(v) => ("Media", s(v)),
        StandardTag::MixDj(v) => ("DJMixer", s(v)),
        StandardTag::MixEngineer(v) => ("Mixer", s(v)),
        StandardTag::Mood(v) => ("Mood", s(v)),
        StandardTag::MovementName(v) => ("MovementName", s(v)),
        StandardTag::MovementTotal(v) => ("MovementTotal", s(v)),
        StandardTag::Mp3GainAlbumMinMax(v) => ("MP3GAIN_ALBUM_MINMAX", s(v)),
        StandardTag::Mp3GainMinMax(v) => ("MP3GAIN_MINMAX", s(v)),
        StandardTag::Mp3GainUndo(v) => ("MP3GAIN_UNDO", s(v)),
        StandardTag::MusicBrainzAlbumArtistId(v) => ("MUSICBRAINZ_ALBUMARTISTID", s(v)),
        StandardTag::MusicBrainzAlbumId(v) => ("MUSICBRAINZ_ALBUMID", s(v)),
        StandardTag::MusicBrainzArtistId(v) => ("MUSICBRAINZ_ARTISTID", s(v)),
        StandardTag::MusicBrainzDiscId(v) => ("MUSICBRAINZ_DISCID", s(v)),
        StandardTag::MusicBrainzOriginalAlbumId(v) => ("MUSICBRAINZ_ORIGINALALBUMID", s(v)),
        StandardTag::MusicBrainzOriginalArtistId(v) => ("MUSICBRAINZ_ORIGINALARTISTID", s(v)),
        StandardTag::MusicBrainzReleaseGroupId(v) => ("MUSICBRAINZ_RELEASEGROUPID", s(v)),
        StandardTag::MusicBrainzReleaseStatus(v) => ("MUSICBRAINZ_ALBUMSTATUS", s(v)),
        StandardTag::MusicBrainzReleaseTrackId(v) => ("MUSICBRAINZ_RELEASETRACKID", s(v)),
        StandardTag::MusicBrainzRecordingId(v) => ("MUSICBRAINZ_TRACKID", s(v)),
        StandardTag::MusicBrainzReleaseType(v) => ("MUSICBRAINZ_ALBUMTYPE", s(v)),
        StandardTag::MusicBrainzTrackId(v) => ("MUSICBRAINZ_TRACKID", s(v)),
        StandardTag::MusicBrainzTrmId(v) => ("MUSICBRAINZ_TRMID", s(v)),
        StandardTag::MusicBrainzWorkId(v) => ("MUSICBRAINZ_WORKID", s(v)),
        StandardTag::OriginalArtist(v) => ("Original Artist", s(v)),
        StandardTag::OriginalFile(v) => ("File", s(v)),
        StandardTag::OriginalReleaseYear(v) => ("OriginalYear", s(v)),
        StandardTag::RecordingDate(v) => ("Record Date", s(v)),
        StandardTag::RecordingLocation(v) => ("Record Location", s(v)),
        StandardTag::ReleaseDate(v) => ("Year", s(v)),
        StandardTag::ReplayGainAlbumGain(v) => ("REPLAYGAIN_ALBUM_GAIN", s(v)),
        StandardTag::ReplayGainAlbumPeak(v) => ("REPLAYGAIN_ALBUM_PEAK", s(v)),
        StandardTag::ReplayGainTrackGain(v) => ("REPLAYGAIN_TRACK_GAIN", s(v)),
        StandardTag::ReplayGainTrackPeak(v) => ("REPLAYGAIN_TRACK_PEAK", s(v)),
        StandardTag::SortAlbum(v) => ("ALBUMSORT", s(v)),
        StandardTag::SortAlbumArtist(v) => ("ALBUMARTISTSORT", s(v)),
        StandardTag::SortArtist(v) => ("ARTISTSORT", s(v)),
        StandardTag::SortComposer(v) => ("COMPOSERSORT", s(v)),
        StandardTag::SortTrackTitle(v) => ("TITLESORT", s(v)),
        StandardTag::TrackSubtitle(v) => ("Subtitle", s(v)),
        StandardTag::TrackTitle(v) => ("Title", s(v)),
        StandardTag::Url(v) => ("Related", s(v)),
        StandardTag::Writer(v) => ("Writer", s(v)),
        _ => return None,
    };

    Some(field)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use symphonia_core::formats::probe::ProbeableMetadata;
    use symphonia_core::io::MediaSourceStream;
    use symphonia_core::meta::{MetadataOptions, RawTag};

    use super::*;
    use crate::ape::ApeReader;

    fn std_tag(key: &str, value: &str, std: StandardTag) -> Tag {
        Tag::new_std(RawTag::new(key, value), std)
    }

    fn arc(s: &str) -> Arc<String> {
        Arc::new(s.to_string())
    }

    fn read_back(tag: Vec<u8>) -> symphonia_core::meta::MetadataRevision {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(tag)), Default::default());
        let mut reader = ApeReader::try_probe_new(mss, MetadataOptions::default()).unwrap();
        reader.read_all().unwrap().revision
    }

    #[test]
    fn verify_empty_input_writes_nothing() {
        assert!(serialize(&[], &[], &[]).is_empty());
    }

    #[test]
    fn verify_header_and_footer() {
        let tag =
            serialize(&[std_tag("x", "Song", StandardTag::TrackTitle(arc("Song")))], &[], &[]);

        let item = 8 + "Title".len() + 1 + "Song".len();
        assert_eq!(tag.len(), 64 + item);
        assert_eq!(&tag[..8], APE_PREAMBLE);
        assert_eq!(read_u32(&tag[8..]).unwrap(), 2000);
        assert_eq!(read_u32(&tag[12..]).unwrap() as usize, item + 32);
        assert_eq!(read_u32(&tag[16..]).unwrap(), 1);
        assert_eq!(read_u32(&tag[20..]).unwrap(), 0xa000_0000);

        let footer = &tag[tag.len() - 32..];
        assert_eq!(&footer[..8], APE_PREAMBLE);
        assert_eq!(&footer[8..20], &tag[8..20]);
        assert_eq!(read_u32(&footer[20..]).unwrap(), 0x8000_0000);
    }

    #[test]
    fn verify_numbers_multi_values_and_unmapped_round_trip() {
        let tags = vec![
            std_tag("a", "Artist One", StandardTag::Artist(arc("Artist One"))),
            std_tag("a", "Artist Two", StandardTag::Artist(arc("Artist Two"))),
            std_tag("t", "3/12", StandardTag::TrackNumber(3)),
            std_tag("t", "3/12", StandardTag::TrackTotal(12)),
            std_tag("d", "2", StandardTag::DiscNumber(2)),
            Tag::new(RawTag::new("MyKey", "my value")),
            Tag::new(RawTag::new("Blob", &[1u8, 2, 3][..])),
            Tag::new(RawTag::new("Link", "https://example.org")),
        ];

        let rev = read_back(serialize(&tags, &[], &["link".to_string()]));

        let find = |f: &dyn Fn(&Tag) -> bool| rev.media.tags.iter().filter(|t| f(t)).count();

        assert_eq!(find(&|t| matches!(&t.std, Some(StandardTag::TrackNumber(3)))), 1);
        assert_eq!(find(&|t| matches!(&t.std, Some(StandardTag::TrackTotal(12)))), 1);
        assert_eq!(find(&|t| matches!(&t.std, Some(StandardTag::DiscNumber(2)))), 1);
        assert_eq!(find(&|t| matches!(&t.std, Some(StandardTag::DiscTotal(_)))), 0);
        assert_eq!(
            find(&|t| matches!(&t.std, Some(StandardTag::Artist(a)) if **a == "Artist One")),
            1
        );
        assert_eq!(
            find(&|t| matches!(&t.std, Some(StandardTag::Artist(a)) if **a == "Artist Two")),
            1
        );
        assert_eq!(
            find(&|t| t.std.is_none()
                && t.raw.key == "MyKey"
                && matches!(&t.raw.value, RawValue::String(v) if **v == "my value")),
            1
        );
        assert_eq!(
            find(&|t| t.raw.key == "Blob"
                && matches!(&t.raw.value, RawValue::Binary(v) if v.as_ref().as_ref() == [1, 2, 3])),
            1
        );
    }

    #[test]
    fn verify_locator_keys_are_found_and_kept() {
        let tags = vec![Tag::new(RawTag::new("Link", "https://example.org"))];
        let tag = serialize(&tags, &[], &["LINK".to_string()]);

        assert_eq!(locator_keys(&tag), vec!["Link".to_string()]);
        assert!(locator_keys(&serialize(&tags, &[], &[])).is_empty());
    }

    #[test]
    fn verify_locator_keys_of_malformed_tags_do_not_panic() {
        let tags = vec![Tag::new(RawTag::new("Link", "https://example.org"))];
        let tag = serialize(&tags, &[], &["link".to_string()]);

        for len in 0..tag.len() {
            let _ = locator_keys(&tag[..len]);
            let _ = locator_keys(&tag[tag.len() - len..]);
        }

        assert!(locator_keys(&[]).is_empty());
        assert!(locator_keys(&[0; 3]).is_empty());
        assert!(locator_keys(&[0; 32]).is_empty());
    }

    #[test]
    fn verify_visuals_round_trip() {
        let png = {
            // Minimal PNG signature + IHDR (1x1, 8-bit RGB); enough for image detection.
            let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 0, 0, 0, 0]);
            v
        };

        let visual = Visual {
            media_type: None,
            dimensions: None,
            color_mode: None,
            usage: Some(StandardVisualKey::BackCover),
            tags: vec![],
            data: png.clone().into_boxed_slice(),
        };

        let rev = read_back(serialize(&[], &[visual], &[]));

        assert_eq!(rev.media.visuals.len(), 1);
        assert_eq!(rev.media.visuals[0].usage, Some(StandardVisualKey::BackCover));
        assert_eq!(rev.media.visuals[0].media_type.as_deref(), Some("image/png"));
        assert_eq!(&*rev.media.visuals[0].data, &png[..]);
    }
}
