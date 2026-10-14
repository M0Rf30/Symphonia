// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Metadata that is not stored as iTunes-style `ilst` items: QuickTime `keys` (`mdta`) metadata,
//! QuickTime user data text atoms (`\u{a9}nam`, ...), and 3GPP asset metadata boxes (`titl`,
//! `perf`, ...).
//!
//! References:
//!  * QuickTime File Format Specification, "Metadata Item Keys Atom" and "User Data Atoms".
//!  * 3GPP TS 26.244 (3GP file format), section 8.3 "Asset information".

use std::sync::Arc;

use symphonia_core::meta::{RawValue, StandardTag, Tag};
use symphonia_core::util::text;

use crate::atoms::ilst::get_raw_tag_key;
use crate::atoms::{Atom, AtomHeader, AtomIterator, AtomType, ReadAtom, Result, decode_error};

/// The maximum size of a user data text atom, or a metadata key, that will be read.
const MAX_TEXT_ATOM_LEN: u64 = 1 << 20;

/// Convert a four-character code to a string. Each byte is interpreted as a Latin-1 character,
/// so `0xa9` is `\u{a9}`.
pub(crate) fn fourcc_to_string(fourcc: [u8; 4]) -> String {
    fourcc.iter().map(|&b| char::from(b)).collect()
}

/// Get the four-character code of an atom type.
pub(crate) fn atom_fourcc(atom_type: AtomType) -> Option<[u8; 4]> {
    let key = match atom_type {
        AtomType::Other(fourcc) => return Some(fourcc),
        atom_type => get_raw_tag_key(atom_type),
    };

    let mut fourcc = [0; 4];
    let mut chars = key.chars();

    for byte in fourcc.iter_mut() {
        *byte = u8::try_from(u32::from(chars.next()?)).ok()?;
    }

    chars.next().is_none().then_some(fourcc)
}

/// A metadata item keys atom (`keys`). The keys are indexed starting at 1 by the items of the
/// sibling `ilst` atom of a `meta` atom with an `mdta` handler.
#[derive(Debug, Default)]
pub struct KeysAtom {
    /// The key of each item, in order. The key namespace is not retained: only keys in the `mdta`
    /// namespace are meaningful to readers, and others are indistinguishable from a reader's
    /// perspective apart from being likely `udta` four-character codes.
    pub keys: Vec<String>,
}

impl Atom for KeysAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;

        let count = it.read_u32()?;

        let mut keys = Vec::new();

        for _ in 0..count {
            // Each key is at least a size and a namespace.
            let size = u64::from(it.read_u32()?);

            if size < 8 || size - 8 > MAX_TEXT_ATOM_LEN {
                return decode_error("isomp4 (keys): invalid key size");
            }

            let namespace = it.read_quad_bytes()?;
            let value = it.read_boxed_slice_exact((size - 8) as usize)?;

            let mut key = String::from_utf8_lossy(&value).into_owned();

            // Keys in the `udta` namespace are four-character codes.
            if &namespace == b"udta" && value.len() == 4 {
                key = fourcc_to_string([value[0], value[1], value[2], value[3]]);
            }

            keys.push(key);
        }

        Ok(KeysAtom { keys })
    }
}

/// The raw payload of an atom, if it is small enough to be reasonable.
pub struct PayloadAtom {
    /// The payload. Empty if the atom was too large.
    pub data: Box<[u8]>,
}

impl Atom for PayloadAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        match header.data_size() {
            Some(size) if size <= MAX_TEXT_ATOM_LEN => {
                Ok(PayloadAtom { data: it.read_boxed_slice_exact(size as usize)? })
            }
            // Too large, or the size is unknown. The metadata is not worth the allocation.
            _ => Ok(PayloadAtom { data: Box::default() }),
        }
    }
}

/// Parse a number that may be formatted as "N" or "N/M".
fn parse_m_of_n(text: &str) -> (Option<u64>, Option<u64>) {
    let mut parts = text.splitn(2, '/');
    let num = parts.next().and_then(|n| n.trim().parse().ok());
    let total = parts.next().and_then(|n| n.trim().parse().ok());
    (num, total)
}

fn arc(text: &str) -> Arc<String> {
    Arc::new(text.to_owned())
}

/// Map a metadata key of a QuickTime metadata item (`mdta` namespace) to a standard tag.
///
/// Keys are reverse-DNS names, usually in the `com.apple.quicktime.` namespace, or simple names
/// (as written by FFmpeg's `use_metadata_tags`). Matching is case-insensitive.
pub(crate) fn mdta_std_tags(key: &str, text: &str) -> [Option<StandardTag>; 2] {
    let key = key.to_ascii_lowercase();
    let key = key.strip_prefix("com.apple.quicktime.").unwrap_or(&key);

    let one = |tag| [Some(tag), None];

    match key {
        "title" | "name" => one(StandardTag::TrackTitle(arc(text))),
        "artist" | "performers" | "performer" => one(StandardTag::Artist(arc(text))),
        "album" => one(StandardTag::Album(arc(text))),
        "album_artist" | "albumartist" => one(StandardTag::AlbumArtist(arc(text))),
        "author" => one(StandardTag::Author(arc(text))),
        "comment" => one(StandardTag::Comment(arc(text))),
        "composer" => one(StandardTag::Composer(arc(text))),
        "conductor" => one(StandardTag::Conductor(arc(text))),
        "copyright" => one(StandardTag::Copyright(arc(text))),
        "description" | "information" => one(StandardTag::Description(arc(text))),
        "director" => one(StandardTag::Director(arc(text))),
        "genre" => one(StandardTag::Genre(arc(text))),
        "grouping" => one(StandardTag::Grouping(arc(text))),
        "keywords" => one(StandardTag::Keywords(arc(text))),
        "language" => one(StandardTag::Language(arc(text))),
        "lyrics" => one(StandardTag::Lyrics(arc(text))),
        "producer" => one(StandardTag::Producer(arc(text))),
        "publisher" | "label" => one(StandardTag::Label(arc(text))),
        "software" | "encoder" => one(StandardTag::Encoder(arc(text))),
        "encoded_by" => one(StandardTag::EncodedBy(arc(text))),
        "creationdate" | "date" | "year" => one(StandardTag::ReleaseDate(arc(text))),
        "track" | "tracknumber" => {
            let (num, total) = parse_m_of_n(text);
            [num.map(StandardTag::TrackNumber), total.map(StandardTag::TrackTotal)]
        }
        "disc" | "discnumber" => {
            let (num, total) = parse_m_of_n(text);
            [num.map(StandardTag::DiscNumber), total.map(StandardTag::DiscTotal)]
        }
        _ => [None, None],
    }
}

/// Mac OS Roman code points for the bytes 0x80..=0xff (bytes below 0x80 are ASCII).
static MAC_ROMAN_HIGH: [char; 128] = [
    '\u{00c4}', '\u{00c5}', '\u{00c7}', '\u{00c9}', '\u{00d1}', '\u{00d6}', '\u{00dc}', '\u{00e1}',
    '\u{00e0}', '\u{00e2}', '\u{00e4}', '\u{00e3}', '\u{00e5}', '\u{00e7}', '\u{00e9}', '\u{00e8}',
    '\u{00ea}', '\u{00eb}', '\u{00ed}', '\u{00ec}', '\u{00ee}', '\u{00ef}', '\u{00f1}', '\u{00f3}',
    '\u{00f2}', '\u{00f4}', '\u{00f6}', '\u{00f5}', '\u{00fa}', '\u{00f9}', '\u{00fb}', '\u{00fc}',
    '\u{2020}', '\u{00b0}', '\u{00a2}', '\u{00a3}', '\u{00a7}', '\u{2022}', '\u{00b6}', '\u{00df}',
    '\u{00ae}', '\u{00a9}', '\u{2122}', '\u{00b4}', '\u{00a8}', '\u{2260}', '\u{00c6}', '\u{00d8}',
    '\u{221e}', '\u{00b1}', '\u{2264}', '\u{2265}', '\u{00a5}', '\u{00b5}', '\u{2202}', '\u{2211}',
    '\u{220f}', '\u{03c0}', '\u{222b}', '\u{00aa}', '\u{00ba}', '\u{03a9}', '\u{00e6}', '\u{00f8}',
    '\u{00bf}', '\u{00a1}', '\u{00ac}', '\u{221a}', '\u{0192}', '\u{2248}', '\u{2206}', '\u{00ab}',
    '\u{00bb}', '\u{2026}', '\u{00a0}', '\u{00c0}', '\u{00c3}', '\u{00d5}', '\u{0152}', '\u{0153}',
    '\u{2013}', '\u{2014}', '\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}', '\u{00f7}', '\u{25ca}',
    '\u{00ff}', '\u{0178}', '\u{2044}', '\u{20ac}', '\u{2039}', '\u{203a}', '\u{fb01}', '\u{fb02}',
    '\u{2021}', '\u{00b7}', '\u{201a}', '\u{201e}', '\u{2030}', '\u{00c2}', '\u{00ca}', '\u{00c1}',
    '\u{00cb}', '\u{00c8}', '\u{00cd}', '\u{00ce}', '\u{00cf}', '\u{00cc}', '\u{00d3}', '\u{00d4}',
    '\u{f8ff}', '\u{00d2}', '\u{00da}', '\u{00db}', '\u{00d9}', '\u{0131}', '\u{02c6}', '\u{02dc}',
    '\u{00af}', '\u{02d8}', '\u{02d9}', '\u{02da}', '\u{00b8}', '\u{02dd}', '\u{02db}', '\u{02c7}',
];

/// Decode the text of a QuickTime user data atom or 3GPP box. Text is UTF-8 or, with a byte order
/// mark, UTF-16. Text that is not valid UTF-8 is decoded as Mac OS Roman, the legacy
/// encoding of QuickTime text for Macintosh language codes. Everything after the first null is
/// removed.
pub(crate) fn decode_text(data: &[u8]) -> String {
    let mut s = match data {
        [0xfe, 0xff, rest @ ..] => text::decode_utf16be_lossy(rest).collect(),
        [0xff, 0xfe, rest @ ..] => text::decode_utf16le_lossy(rest).collect(),
        _ => match std::str::from_utf8(data) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                data.iter()
                    .map(|&b| {
                        if b < 0x80 { char::from(b) } else { MAC_ROMAN_HIGH[usize::from(b - 0x80)] }
                    })
                    .collect()
            }
        },
    };

    // Remove (the first) null terminator and everything after it.
    if let Some(end) = s.find('\0') {
        s.truncate(end);
    }

    s
}

/// Map a QuickTime user data text atom (`\u{a9}xxx`) to a standard tag.
fn qt_udta_std_tag(key: &str, text: &str) -> Option<StandardTag> {
    let text = arc(text);

    Some(match key {
        "\u{a9}nam" => StandardTag::TrackTitle(text),
        "\u{a9}ART" => StandardTag::Artist(text),
        "\u{a9}alb" => StandardTag::Album(text),
        "\u{a9}aut" => StandardTag::Author(text),
        "\u{a9}cmt" => StandardTag::Comment(text),
        "\u{a9}com" | "\u{a9}wrt" => StandardTag::Composer(text),
        "\u{a9}cpy" => StandardTag::Copyright(text),
        "\u{a9}day" => StandardTag::ReleaseDate(text),
        "\u{a9}des" | "\u{a9}inf" => StandardTag::Description(text),
        "\u{a9}dir" => StandardTag::Director(text),
        "\u{a9}enc" => StandardTag::EncodedBy(text),
        "\u{a9}gen" => StandardTag::Genre(text),
        "\u{a9}grp" => StandardTag::Grouping(text),
        "\u{a9}lyr" => StandardTag::Lyrics(text),
        "\u{a9}prd" => StandardTag::Producer(text),
        "\u{a9}prf" => StandardTag::Performer(text),
        "\u{a9}pub" => StandardTag::Label(text),
        "\u{a9}swr" | "\u{a9}too" => StandardTag::Encoder(text),
        _ => return None,
    })
}

/// Parse a QuickTime user data text atom: a 16-bit text size, a 16-bit language code, and the
/// text.
pub(crate) fn parse_qt_udta_text(key: &str, data: &[u8]) -> Option<Tag> {
    let (header, rest) = data.split_at_checked(4)?;

    let size = usize::from(u16::from_be_bytes([header[0], header[1]]));
    let text = decode_text(&rest[..size.min(rest.len())]);

    Some(Tag::new_from_parts(key, RawValue::from(text.clone()), qt_udta_std_tag(key, &text)))
}

/// Gets if the four-character code is a 3GPP asset metadata box that holds text.
pub(crate) fn is_3gpp_text_box(fourcc: [u8; 4]) -> bool {
    matches!(
        &fourcc,
        b"titl"
            | b"dscp"
            | b"cprt"
            | b"perf"
            | b"auth"
            | b"gnre"
            | b"albm"
            | b"yrrc"
            | b"rtng"
            | b"clsf"
            | b"kywd"
    )
}

/// Gets if a box of a user data (`udta`) atom is read as a tag by the reader: a QuickTime text
/// atom (`\u{a9}xxx`) or a 3GPP asset metadata box.
#[cfg(feature = "writer")]
pub fn is_udta_tag_box(fourcc: [u8; 4]) -> bool {
    fourcc[0] == 0xa9 || is_3gpp_text_box(fourcc)
}

/// Parse a 3GPP asset metadata box (TS 26.244, 8.3). `data` is the payload including the full
/// box header (version and flags). Returns the tags the box describes.
pub(crate) fn parse_3gpp_box(fourcc: [u8; 4], data: &[u8], tags: &mut Vec<Tag>) {
    let key = fourcc_to_string(fourcc);

    // Skip the version and flags.
    let Some(data) = data.get(4..)
    else {
        return;
    };

    // The `yrrc` box contains only a year.
    if &fourcc == b"yrrc" {
        if let [hi, lo, ..] = data {
            let year = u16::from_be_bytes([*hi, *lo]);
            tags.push(Tag::new_from_parts(
                key,
                RawValue::from(u64::from(year)),
                Some(StandardTag::RecordingYear(year)),
            ));
        }
        return;
    }

    // The rating and classification boxes have a rating/classification entity (4 bytes) and
    // criteria/table (4 bytes) preceding the language.
    let data = match &fourcc {
        b"rtng" | b"clsf" => match data.get(8..) {
            Some(data) => data,
            None => return,
        },
        _ => data,
    };

    // The language is a padding bit and 15 bits of packed ISO-639-2/T.
    let Some(data) = data.get(2..)
    else {
        return;
    };

    // The keyword box has a count of keywords, each a length-prefixed string.
    if &fourcc == b"kywd" {
        let Some((&count, mut rest)) = data.split_first()
        else {
            return;
        };

        let mut words = Vec::new();

        for _ in 0..count {
            let Some((&len, tail)) = rest.split_first()
            else {
                break;
            };

            let len = usize::from(len).min(tail.len());
            words.push(decode_text(&tail[..len]));
            rest = &tail[len..];
        }

        let text = words.join(", ");
        tags.push(Tag::new_from_parts(
            key,
            RawValue::from(text.clone()),
            Some(StandardTag::Keywords(Arc::new(text))),
        ));
        return;
    }

    // The text is UTF-8 or UTF-16 with a byte order mark, and null-terminated. The album box has
    // an optional track number following the terminator.
    let (text, track) = match &fourcc {
        b"albm" => split_text_and_track(data),
        _ => (decode_text(data), None),
    };

    let std = match &fourcc {
        b"titl" => Some(StandardTag::TrackTitle(arc(&text))),
        b"dscp" => Some(StandardTag::Description(arc(&text))),
        b"cprt" => Some(StandardTag::Copyright(arc(&text))),
        b"perf" => Some(StandardTag::Artist(arc(&text))),
        b"auth" => Some(StandardTag::Author(arc(&text))),
        b"gnre" => Some(StandardTag::Genre(arc(&text))),
        b"albm" => Some(StandardTag::Album(arc(&text))),
        _ => None,
    };

    tags.push(Tag::new_from_parts(key.clone(), RawValue::from(text), std));

    if let Some(track) = track {
        tags.push(Tag::new_from_parts(
            key,
            RawValue::from(u64::from(track)),
            Some(StandardTag::TrackNumber(u64::from(track))),
        ));
    }
}

/// Split the payload of an album box into its text, and the track number if present.
fn split_text_and_track(data: &[u8]) -> (String, Option<u8>) {
    // The terminator is a single null byte for UTF-8 and a null code unit for UTF-16. Find the
    // end of the text.
    let utf16 = matches!(data, [0xfe, 0xff, ..] | [0xff, 0xfe, ..]);

    let end = if utf16 {
        data.chunks_exact(2).position(|c| c == [0, 0]).map(|p| 2 * p + 2)
    }
    else {
        data.iter().position(|&b| b == 0).map(|p| p + 1)
    };

    match end {
        Some(end) => (decode_text(&data[..end]), data.get(end).copied()),
        None => (decode_text(data), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_text_is_mac_roman() {
        // "Café – 100€" in Mac OS Roman.
        assert_eq!(decode_text(b"Caf\x8e \xd0 100\xdb"), "Caf\u{e9} \u{2013} 100\u{20ac}");
        assert_eq!(decode_text("Caf\u{e9}".as_bytes()), "Caf\u{e9}");
    }

    #[test]
    fn mdta_keys_map_to_standard_tags() {
        let [first, _] = mdta_std_tags("com.apple.quicktime.title", "T");
        assert!(matches!(first, Some(StandardTag::TrackTitle(t)) if t.as_str() == "T"));

        let [num, total] = mdta_std_tags("track", "3/12");
        assert!(matches!(num, Some(StandardTag::TrackNumber(3))));
        assert!(matches!(total, Some(StandardTag::TrackTotal(12))));

        assert_eq!(mdta_std_tags("com.example.unknown", "x"), [None, None]);
    }

    #[test]
    fn qt_udta_text_is_read() {
        let mut data = vec![0, 5, 0x55, 0xc4];
        data.extend_from_slice(b"Title");
        let tag = parse_qt_udta_text("\u{a9}nam", &data).expect("tag");
        assert!(matches!(tag.std, Some(StandardTag::TrackTitle(t)) if t.as_str() == "Title"));
    }

    #[test]
    fn threegpp_album_has_track_number() {
        // Version/flags, language, "Alb\0", track 7.
        let mut data = vec![0, 0, 0, 0, 0x55, 0xc4];
        data.extend_from_slice(b"Alb\0\x07");
        let mut tags = Vec::new();
        parse_3gpp_box(*b"albm", &data, &mut tags);
        assert_eq!(tags.len(), 2);
        assert!(matches!(&tags[0].std, Some(StandardTag::Album(a)) if a.as_str() == "Alb"));
        assert!(matches!(tags[1].std, Some(StandardTag::TrackNumber(7))));
    }
}
