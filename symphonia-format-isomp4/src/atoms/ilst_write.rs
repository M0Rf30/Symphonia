// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Serializer for the iTunes-style metadata item list (`ilst`).
//!
//! The mapping from tags to atoms is the reverse of the one [`IlstAtom`](super::ilst::IlstAtom)
//! and the iTunes free-form table use when reading, so a [`Tag`] written here is read back as the
//! same [`StandardTag`].
//!
//! * A tag with a standard tag is written to the atom the reader maps to that standard tag. Where
//!   the reader accepts several atoms (`desc`/`ldes`, `\u{a9}wrt`/`\u{a9}com`, ...), the one
//!   Picard writes is used, unless the tag's raw key names one of the others, in which case that
//!   atom is kept. Standard tags that only exist as iTunes free-form atoms (MusicBrainz
//!   identifiers, `ARTISTS`, `ISRC`, `LABEL`, ...) are written to a `----` atom named as Picard
//!   names them.
//! * A tag without a standard tag is written to the atom its raw key names, if that is a known
//!   atom (`pgap`, `apID`, ...), or else to a `----` atom (mean `com.apple.iTunes` unless the key
//!   is `<reverse.dns>:<name>`) named after the raw key.
//! * Tags written to the same atom become several `data` atoms of that atom.
//! * `trkn` and `disk` combine the number and total standard tags into the native pair.
//! * Every [`Visual`] becomes one `data` atom of `covr`.
//! * Atoms the tag model does not represent (unknown atoms, `hdvd`) are preserved from the
//!   existing `ilst`.

use symphonia_core::errors::{Error, Result};
use symphonia_core::meta::{ContentAdvisory, RawTag, RawValue, StandardTag, Tag, Visual};

use log::{debug, warn};

use crate::atoms::AtomType;
use crate::atoms::ilst::get_raw_tag_key;

/// The reverse-DNS namespace iTunes (and Picard) use for free-form atoms.
const ITUNES_MEAN: &str = "com.apple.iTunes";

// Well-known data types of the `data` atom.
const TYPE_NONE: u32 = 0;
const TYPE_UTF8: u32 = 1;
const TYPE_JPEG: u32 = 13;
const TYPE_PNG: u32 = 14;
const TYPE_INT: u32 = 21;
const TYPE_UINT: u32 = 22;
const TYPE_FLOAT64: u32 = 24;
const TYPE_BMP: u32 = 27;

const TRKN: [u8; 4] = *b"trkn";
const DISK: [u8; 4] = *b"disk";
const GNRE: [u8; 4] = *b"gnre";
const COVR: [u8; 4] = *b"covr";
const STIK: [u8; 4] = *b"stik";

/// The (mean, name) pair or the four character code an item is stored under.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Slot {
    Atom([u8; 4]),
    Free { mean: String, name: String },
}

impl Slot {
    fn atom(fourcc: &[u8; 4]) -> Slot {
        Slot::Atom(*fourcc)
    }

    fn free(name: &str) -> Slot {
        Slot::Free { mean: ITUNES_MEAN.to_string(), name: name.to_string() }
    }

    /// Whether both slots name the same field. The reader looks free-form names up
    /// case-insensitively, so they compare the same way.
    fn same_field(&self, other: &Slot) -> bool {
        match (self, other) {
            (Slot::Atom(a), Slot::Atom(b)) => a == b,
            (Slot::Free { mean: m0, name: n0 }, Slot::Free { mean: m1, name: n1 }) => {
                m0.eq_ignore_ascii_case(m1) && n0.eq_ignore_ascii_case(n1)
            }
            _ => false,
        }
    }
}

/// One `data` atom: a well-known data type and its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Datum {
    data_type: u32,
    bytes: Vec<u8>,
}

impl Datum {
    fn text(s: &str) -> Datum {
        Datum { data_type: TYPE_UTF8, bytes: s.as_bytes().to_vec() }
    }

    /// A big-endian integer at least `min_width` bytes wide (1, 2, 4 or 8).
    fn int(value: i128, min_width: usize) -> Datum {
        if value > i128::from(i64::MAX) {
            let value = u64::try_from(value).unwrap_or(u64::MAX);
            return Datum { data_type: TYPE_UINT, bytes: value.to_be_bytes().to_vec() };
        }

        let value = i64::try_from(value.max(i128::from(i64::MIN))).unwrap_or(0);

        let width = if i8::try_from(value).is_ok() {
            1
        }
        else if i16::try_from(value).is_ok() {
            2
        }
        else if i32::try_from(value).is_ok() {
            4
        }
        else {
            8
        };

        let width = width.max(min_width);

        Datum { data_type: TYPE_INT, bytes: value.to_be_bytes()[8 - width..].to_vec() }
    }
}

/// An atom (or free-form atom) and its values.
struct Item {
    slot: Slot,
    values: Vec<Datum>,
}

/// The number/total pair of `trkn` or `disk`.
#[derive(Default)]
struct Pair {
    number: Option<u64>,
    total: Option<u64>,
}

impl Pair {
    fn datum(&self, atom: &[u8; 4]) -> Option<Datum> {
        let fit = |v: Option<u64>| match v.map(u16::try_from) {
            Some(Ok(v)) => v,
            Some(Err(_)) => {
                warn!("isomp4: {} does not fit in 16 bits, dropped", fourcc_lossy(atom));
                0
            }
            None => 0,
        };

        let (number, total) = (fit(self.number), fit(self.total));

        if number == 0 && total == 0 {
            return None;
        }

        // `trkn` is {reserved, number, total, reserved}, `disk` is {reserved, number, total}.
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&number.to_be_bytes());
        bytes.extend_from_slice(&total.to_be_bytes());

        if atom == &TRKN {
            bytes.extend_from_slice(&[0, 0]);
        }

        Some(Datum { data_type: TYPE_NONE, bytes })
    }
}

fn fourcc_lossy(fourcc: &[u8; 4]) -> String {
    fourcc.iter().map(|&b| char::from(b)).collect()
}

/// Serializes `tags` and `visuals` into an `ilst` atom (including its header), keeping the atoms of
/// `existing` that the tag model does not represent.
///
/// `existing` is the payload (the child atoms) of the file's current `ilst` atom, or empty.
///
/// Returns `None` if there is nothing to store, in which case the file should carry no `ilst`.
pub fn build_ilst(existing: &[u8], tags: &[Tag], visuals: &[Visual]) -> Result<Option<Vec<u8>>> {
    let mut items: Vec<Item> = Vec::new();
    let mut track = Pair::default();
    let mut disc = Pair::default();

    for tag in tags {
        // Track and disc are stored as one atom holding the number and the total.
        let pair = match &tag.std {
            Some(StandardTag::TrackNumber(n)) => Some((&mut track, TRKN, Some(*n), None)),
            Some(StandardTag::TrackTotal(t)) => Some((&mut track, TRKN, None, Some(*t))),
            Some(StandardTag::DiscNumber(n)) => Some((&mut disc, DISK, Some(*n), None)),
            Some(StandardTag::DiscTotal(t)) => Some((&mut disc, DISK, None, Some(*t))),
            _ => None,
        };

        if let Some((pair, atom, number, total)) = pair {
            // The first of each wins.
            if number.is_some() && pair.number.is_none() {
                pair.number = number;
            }
            if total.is_some() && pair.total.is_none() {
                pair.total = total;
            }
            // Reserve the atom's position; the value is filled in below.
            if !items.iter().any(|item| item.slot == Slot::Atom(atom)) {
                items.push(Item { slot: Slot::Atom(atom), values: Vec::new() });
            }
            continue;
        }

        match resolve(tag) {
            Some((slot, values)) => push_values(&mut items, slot, values),
            None => debug!("isomp4: tag '{}' has no ilst representation, dropped", tag.raw.key),
        }
    }

    for item in &mut items {
        if let Slot::Atom(atom) = &item.slot {
            let pair = if atom == &TRKN {
                &track
            }
            else if atom == &DISK {
                &disc
            }
            else {
                continue;
            };
            item.values = pair.datum(atom).into_iter().collect();
        }
    }

    if !visuals.is_empty() {
        let values = visuals.iter().map(cover_datum).collect::<Result<Vec<_>>>()?;
        items.push(Item { slot: Slot::Atom(COVR), values });
    }

    let mut children = Vec::new();

    for item in items.iter().filter(|item| !item.values.is_empty()) {
        write_item(&mut children, item)?;
    }

    copy_unmanaged_atoms(existing, &mut children);

    if children.is_empty() {
        return Ok(None);
    }

    let mut ilst = Vec::with_capacity(children.len() + 8);
    let at = begin_atom(&mut ilst, b"ilst");
    ilst.extend_from_slice(&children);
    end_atom(&mut ilst, at)?;

    Ok(Some(ilst))
}

/// The `hdlr` atom of an iTunes-style `meta` atom: handler type `mdir` by manufacturer `appl`.
pub fn mdir_hdlr_box() -> Vec<u8> {
    let mut hdlr = Vec::with_capacity(33);
    hdlr.extend_from_slice(&33u32.to_be_bytes());
    hdlr.extend_from_slice(b"hdlr");
    // Version and flags, then the QuickTime component type (unused).
    hdlr.extend_from_slice(&[0; 8]);
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    // Component flags and mask, then the empty name.
    hdlr.extend_from_slice(&[0; 9]);
    hdlr
}

/// Adds `values` to the item of `slot`, creating it if needed. Text atoms may hold several
/// distinct values; any other atom holds one.
fn push_values(items: &mut Vec<Item>, slot: Slot, values: Vec<Datum>) {
    let index = match items.iter().position(|item| item.slot == slot) {
        Some(index) => index,
        None => {
            items.push(Item { slot, values: Vec::new() });
            items.len() - 1
        }
    };

    let item = &mut items[index];

    for value in values {
        let multi_valued =
            value.data_type == TYPE_UTF8 && item.values.iter().all(|v| v.data_type == TYPE_UTF8);

        if (item.values.is_empty() || multi_valued) && !item.values.contains(&value) {
            item.values.push(value);
        }
    }
}

/// Resolves a tag to the slot it is stored in and its values.
fn resolve(tag: &Tag) -> Option<(Slot, Vec<Datum>)> {
    let raw_slot = raw_slot(&tag.raw.key);

    if let Some(std) = &tag.std {
        let mapped = match (std, &raw_slot) {
            // The media kind is a number in `stik`, but the media format of a release (CD, ...)
            // is free-form text.
            (StandardTag::MediaFormat(name), Some(Slot::Atom(STIK))) => media_kind_number(name)
                .or_else(|| raw_integer(&tag.raw.value))
                .map(|n| (Slot::Atom(STIK), Datum::int(n, 1))),
            _ => classify(std),
        };

        if let Some((canonical, datum)) = mapped {
            // Keep the atom (or the casing of the free-form name) the tag was read from, if the
            // reader maps that to the same standard tag.
            let slot = match raw_slot {
                Some(slot) if slot.same_field(&canonical) || is_alias(std, &slot) => slot,
                _ => canonical,
            };

            return Some((slot, vec![datum]));
        }
    }

    // No native mapping: keep the tag under its raw key.
    let slot = raw_slot?;

    if let Slot::Atom(atom) = &slot {
        if matches!(*atom, TRKN | DISK | GNRE | COVR) {
            return None;
        }
    }

    let width = match &slot {
        Slot::Atom(atom) => int_width(atom),
        Slot::Free { .. } => 1,
    };

    let values = raw_data(&tag.raw, width);

    if values.is_empty() { None } else { Some((slot, values)) }
}

/// Natural width of the integer atoms.
fn int_width(atom: &[u8; 4]) -> usize {
    match atom {
        b"plID" => 8,
        b"atID" | b"cmID" | b"cnID" | b"geID" | b"sfID" | b"tves" | b"tvsn" => 4,
        b"tmpo" | b"\xa9mvc" | b"\xa9mvi" => 2,
        _ => 1,
    }
}

fn raw_integer(value: &RawValue) -> Option<i128> {
    match value {
        RawValue::SignedInt(v) => Some(i128::from(*v)),
        RawValue::UnsignedInt(v) => Some(i128::from(*v)),
        _ => None,
    }
}

/// Encodes a raw tag's value as `data` atoms.
fn raw_data(raw: &RawTag, int_width: usize) -> Vec<Datum> {
    match &raw.value {
        RawValue::String(s) => vec![Datum::text(s)],
        RawValue::StringList(list) => list.iter().map(|s| Datum::text(s)).collect(),
        RawValue::Binary(b) => vec![Datum { data_type: TYPE_NONE, bytes: b.to_vec() }],
        RawValue::Boolean(b) => vec![Datum::int(i128::from(*b), int_width)],
        RawValue::Flag => vec![Datum::int(1, int_width)],
        RawValue::SignedInt(v) => vec![Datum::int(i128::from(*v), int_width)],
        RawValue::UnsignedInt(v) => vec![Datum::int(i128::from(*v), int_width)],
        RawValue::Float(v) => {
            vec![Datum { data_type: TYPE_FLOAT64, bytes: v.to_be_bytes().to_vec() }]
        }
        _ => Vec::new(),
    }
}

/// Maps a raw key to the slot it names: a known atom, or else a free-form atom.
fn raw_slot(key: &str) -> Option<Slot> {
    if let Some(name) = key.strip_prefix("----:") {
        return free_slot(name);
    }

    if let Some(atom) = key_fourcc(key) {
        if atom != *b"----" && is_managed_atom(&atom) {
            return Some(Slot::Atom(atom));
        }
    }

    free_slot(key)
}

/// A free-form slot from `<name>` (under the iTunes mean) or `<reverse.dns.mean>:<name>`.
fn free_slot(key: &str) -> Option<Slot> {
    if key.is_empty() || key == "----" {
        return None;
    }

    if let Some((mean, name)) = key.split_once(':') {
        if mean.contains('.') && !mean.contains(char::is_whitespace) && !name.is_empty() {
            return Some(Slot::Free { mean: mean.to_string(), name: name.to_string() });
        }
    }

    Some(Slot::free(key))
}

/// The four character code a raw key spells, if it is one (each character is one byte of the
/// code, so `\u{a9}` is 0xa9).
fn key_fourcc(key: &str) -> Option<[u8; 4]> {
    let mut fourcc = [0u8; 4];
    let mut chars = key.chars();

    for byte in &mut fourcc {
        *byte = u8::try_from(u32::from(chars.next()?)).ok()?;
    }

    chars.next().is_none().then_some(fourcc)
}

/// Whether the reader turns this atom into tags (and visuals), so that it is rewritten from the
/// tags instead of preserved.
fn is_managed_atom(atom: &[u8; 4]) -> bool {
    let atom_type = AtomType::from(*atom);

    // `hdvd` is a recognized atom that the reader does not turn into a tag.
    !matches!(atom_type, AtomType::Other(_) | AtomType::HdVideoTag)
        && !get_raw_tag_key(atom_type).is_empty()
}

/// The atom `std` is written to, and its value.
///
/// This is the reverse of the reader's mapping. Where the reader accepts several atoms, this is
/// the one Picard writes. Standard tags the reader has no atom for return `None`.
fn classify(std: &StandardTag) -> Option<(Slot, Datum)> {
    use StandardTag as S;

    let atom_text = |atom: &[u8; 4], v: &str| Some((Slot::atom(atom), Datum::text(v)));
    let atom_int = |atom: &[u8; 4], v: u64| {
        Some((Slot::atom(atom), Datum::int(i128::from(v), int_width(atom))))
    };
    let atom_flag = |atom: &[u8; 4], v: bool| atom_int(atom, u64::from(v));

    match std {
        S::Album(v) => atom_text(b"\xa9alb", v),
        S::AlbumArtist(v) => atom_text(b"aART", v),
        S::Arranger(v) => atom_text(b"\xa9arg", v),
        S::Artist(v) => atom_text(b"\xa9ART", v),
        S::Author(v) => atom_text(b"\xa9aut", v),
        S::Comment(v) => atom_text(b"\xa9cmt", v),
        S::Composer(v) => atom_text(b"\xa9wrt", v),
        S::Copyright(v) => atom_text(b"cprt", v),
        S::Description(v) => atom_text(b"desc", v),
        S::EncodedBy(v) => atom_text(b"\xa9enc", v),
        S::Encoder(v) => atom_text(b"\xa9too", v),
        S::Genre(v) => atom_text(b"\xa9gen", v),
        S::Grouping(v) => atom_text(b"\xa9grp", v),
        S::IdentPodcast(v) => atom_text(b"egid", v),
        S::Lyrics(v) => atom_text(b"\xa9lyr", v),
        S::MovementName(v) => atom_text(b"\xa9mvn", v),
        S::Narrator(v) => atom_text(b"\xa9nrt", v),
        S::OriginalArtist(v) => atom_text(b"\xa9ope", v),
        S::Owner(v) => atom_text(b"ownr", v),
        S::PodcastCategory(v) => atom_text(b"catg", v),
        S::PodcastKeywords(v) => atom_text(b"keyw", v),
        S::ProductionCopyright(v) => atom_text(b"\xa9phg", v),
        S::PurchaseDate(v) => atom_text(b"purd", v),
        S::ReleaseDate(v) => atom_text(b"\xa9day", v),
        S::Soloist(v) => atom_text(b"\xa9sol", v),
        S::SortAlbum(v) => atom_text(b"soal", v),
        S::SortAlbumArtist(v) => atom_text(b"soaa", v),
        S::SortArtist(v) => atom_text(b"soar", v),
        S::SortComposer(v) => atom_text(b"soco", v),
        S::SortTrackTitle(v) => atom_text(b"sonm", v),
        S::SortTvSeriesTitle(v) => atom_text(b"sosn", v),
        S::TrackTitle(v) => atom_text(b"\xa9nam", v),
        S::TvEpisodeTitle(v) => atom_text(b"tven", v),
        S::TvNetwork(v) => atom_text(b"tvnn", v),
        S::TvSeriesTitle(v) => atom_text(b"tvsh", v),
        S::UrlArtist(v) => atom_text(b"\xa9prl", v),
        S::UrlLabel(v) => atom_text(b"\xa9lal", v),
        S::UrlPodcast(v) => atom_text(b"purl", v),
        S::Work(v) => atom_text(b"\xa9wrk", v),

        S::Bpm(v) => atom_int(b"tmpo", *v),
        S::MovementNumber(v) => atom_int(b"\xa9mvi", *v),
        S::MovementTotal(v) => atom_int(b"\xa9mvc", *v),
        S::TvEpisodeNumber(v) => atom_int(b"tves", *v),
        S::TvSeasonNumber(v) => atom_int(b"tvsn", *v),

        S::CompilationFlag(v) => atom_flag(b"cpil", *v),
        S::PodcastFlag(v) => atom_flag(b"pcst", *v),

        // The rating is stored as a percentage.
        S::Rating(ppm) => atom_int(b"rate", u64::from((*ppm / 10_000).min(100))),
        S::ContentAdvisory(advisory) => atom_int(
            b"rtng",
            match advisory {
                ContentAdvisory::None => 0,
                ContentAdvisory::Explicit => 1,
                ContentAdvisory::Censored => 2,
            },
        ),

        S::OriginalReleaseYear(year) => {
            Some((Slot::free("ORIGINALYEAR"), Datum::text(&year.to_string())))
        }

        _ => {
            let (name, value) = freeform_name(std)?;
            Some((Slot::free(name), Datum::text(value)))
        }
    }
}

/// The free-form name Picard gives a standard tag that has no atom of its own, and its value.
fn freeform_name(std: &StandardTag) -> Option<(&'static str, &str)> {
    use StandardTag as S;

    let pair = match std {
        S::AcoustIdFingerprint(v) => ("Acoustid Fingerprint", v),
        S::AcoustIdId(v) => ("Acoustid Id", v),
        S::IdentAsin(v) => ("ASIN", v),
        S::IdentBarcode(v) => ("BARCODE", v),
        S::IdentCatalogNumber(v) => ("CATALOGNUMBER", v),
        S::Conductor(v) => ("CONDUCTOR", v),
        S::DiscSubtitle(v) => ("DISCSUBTITLE", v),
        S::MixDj(v) => ("DJMIXER", v),
        S::Engineer(v) => ("ENGINEER", v),
        S::InitialKey(v) => ("initialkey", v),
        S::IdentIsrc(v) => ("ISRC", v),
        S::Label(v) => ("LABEL", v),
        S::Language(v) => ("LANGUAGE", v),
        S::License(v) => ("LICENSE", v),
        S::Lyricist(v) => ("LYRICIST", v),
        S::MediaFormat(v) => ("MEDIA", v),
        S::MixEngineer(v) => ("MIXER", v),
        S::Mood(v) => ("MOOD", v),
        S::MusicBrainzAlbumArtistId(v) => ("MusicBrainz Album Artist Id", v),
        S::MusicBrainzAlbumId(v) => ("MusicBrainz Album Id", v),
        S::ReleaseCountry(v) => ("MusicBrainz Album Release Country", v),
        S::MusicBrainzReleaseStatus(v) => ("MusicBrainz Album Status", v),
        S::MusicBrainzReleaseType(v) => ("MusicBrainz Album Type", v),
        S::MusicBrainzArtistId(v) => ("MusicBrainz Artist Id", v),
        S::MusicBrainzDiscId(v) => ("MusicBrainz Disc Id", v),
        S::MusicBrainzOriginalAlbumId(v) => ("MusicBrainz Original Album Id", v),
        S::MusicBrainzOriginalArtistId(v) => ("MusicBrainz Original Artist Id", v),
        S::MusicBrainzReleaseGroupId(v) => ("MusicBrainz Release Group Id", v),
        S::MusicBrainzReleaseTrackId(v) => ("MusicBrainz Release Track Id", v),
        // Picard stores the recording id in the "track id" field. Both read back as the track id.
        S::MusicBrainzRecordingId(v) | S::MusicBrainzTrackId(v) => ("MusicBrainz Track Id", v),
        S::MusicBrainzTrmId(v) => ("MusicBrainz TRM Id", v),
        S::MusicBrainzWorkId(v) => ("MusicBrainz Work Id", v),
        S::OriginalReleaseDate(v) => ("ORIGINALDATE", v),
        S::Producer(v) => ("PRODUCER", v),
        S::Remixer(v) => ("REMIXER", v),
        S::ReplayGainAlbumGain(v) => ("replaygain_album_gain", v),
        S::ReplayGainAlbumPeak(v) => ("replaygain_album_peak", v),
        S::ReplayGainAlbumRange(v) => ("replaygain_album_range", v),
        S::ReplayGainReferenceLoudness(v) => ("replaygain_reference_loudness", v),
        S::ReplayGainTrackGain(v) => ("replaygain_track_gain", v),
        S::ReplayGainTrackPeak(v) => ("replaygain_track_peak", v),
        S::ReplayGainTrackRange(v) => ("replaygain_track_range", v),
        S::Script(v) => ("SCRIPT", v),
        S::TrackSubtitle(v) => ("SUBTITLE", v),
        S::Writer(v) => ("WRITER", v),
        _ => return None,
    };

    Some((pair.0, pair.1.as_str()))
}

/// Whether the reader maps `slot` to the same standard tag as the one [`classify`] picks for
/// `std`.
fn is_alias(std: &StandardTag, slot: &Slot) -> bool {
    use StandardTag as S;

    match slot {
        Slot::Atom(atom) => match std {
            S::Description(_) => atom == b"ldes",
            S::Composer(_) => atom == b"\xa9com",
            S::Conductor(_) => atom == b"\xa9con",
            S::IdentIsrc(_) => atom == b"\xa9isr",
            S::Label(_) => atom == b"\xa9lab" || atom == b"\xa9pub",
            S::Producer(_) => atom == b"\xa9prd",
            _ => false,
        },
        Slot::Free { mean, name } => {
            mean.eq_ignore_ascii_case(ITUNES_MEAN)
                && match std {
                    S::Artist(_) => name.eq_ignore_ascii_case("ARTISTS"),
                    S::ReleaseDate(_) => name.eq_ignore_ascii_case("RELEASEDATE"),
                    _ => false,
                }
        }
    }
}

/// The `stik` number of a media kind name the reader produces.
fn media_kind_number(name: &str) -> Option<i128> {
    match name {
        "Movie" => Some(0),
        "Normal" => Some(1),
        "Audio Book" => Some(2),
        "Whacked Bookmark" => Some(5),
        "Music Video" => Some(6),
        "Short Film" => Some(9),
        "TV Show" => Some(10),
        "Booklet" => Some(11),
        _ => None,
    }
}

/// The `covr` data atom of a visual. Only the three image formats iTunes knows are allowed.
fn cover_datum(visual: &Visual) -> Result<Datum> {
    let data = &visual.data;

    let sniffed = if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(TYPE_JPEG)
    }
    else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(TYPE_PNG)
    }
    else if data.starts_with(b"BM") {
        Some(TYPE_BMP)
    }
    else {
        None
    };

    let declared = visual.media_type.as_deref().and_then(|media_type| {
        match media_type.to_ascii_lowercase().as_str() {
            "image/jpeg" | "image/jpg" => Some(TYPE_JPEG),
            "image/png" => Some(TYPE_PNG),
            "image/bmp" => Some(TYPE_BMP),
            _ => None,
        }
    });

    match sniffed.or(declared) {
        Some(data_type) => Ok(Datum { data_type, bytes: data.to_vec() }),
        None => Err(Error::Unsupported("isomp4: cover art must be a JPEG, PNG or BMP image")),
    }
}

/// Appends the atoms of `existing` (the payload of an `ilst`) that the reader does not turn into
/// tags to `out`, as they are.
fn copy_unmanaged_atoms(existing: &[u8], out: &mut Vec<u8>) {
    let mut pos = 0usize;

    while let Some(header) = existing.get(pos..pos + 8) {
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let fourcc = [header[4], header[5], header[6], header[7]];

        let (header_len, size) = match size32 {
            // The atom extends to the end of the list.
            0 => (8, existing.len() - pos),
            1 => match existing.get(pos + 8..pos + 16) {
                Some(large) => {
                    let mut be = [0u8; 8];
                    be.copy_from_slice(large);
                    match usize::try_from(u64::from_be_bytes(be)) {
                        Ok(size) => (16, size),
                        Err(_) => break,
                    }
                }
                None => break,
            },
            size => (8, size as usize),
        };

        if size < header_len || size > existing.len() - pos {
            warn!("isomp4: truncated atom in ilst, ignoring the rest");
            break;
        }

        if !is_managed_atom(&fourcc) {
            if size32 == 0 {
                // Give the atom the explicit size it would otherwise lose when it stops being
                // the last one.
                out.extend_from_slice(&(size as u32).to_be_bytes());
                out.extend_from_slice(&existing[pos + 4..pos + size]);
            }
            else {
                out.extend_from_slice(&existing[pos..pos + size]);
            }
        }

        pos += size;
    }
}

/// Serializes an item as an atom.
fn write_item(out: &mut Vec<u8>, item: &Item) -> Result<()> {
    let at = match &item.slot {
        Slot::Atom(atom) => begin_atom(out, atom),
        Slot::Free { mean, name } => {
            let at = begin_atom(out, b"----");

            for (fourcc, text) in [(b"mean", mean), (b"name", name)] {
                let at = begin_atom(out, fourcc);
                out.extend_from_slice(&[0; 4]);
                out.extend_from_slice(text.as_bytes());
                end_atom(out, at)?;
            }

            at
        }
    };

    for value in &item.values {
        let data_at = begin_atom(out, b"data");
        // Version 0, and the data type as the flags.
        out.push(0);
        out.extend_from_slice(&value.data_type.to_be_bytes()[1..]);
        // Locale: default country and language.
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&value.bytes);
        end_atom(out, data_at)?;
    }

    end_atom(out, at)
}

/// Starts an atom: writes its header with a placeholder size and returns its position.
fn begin_atom(out: &mut Vec<u8>, fourcc: &[u8; 4]) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(fourcc);
    at
}

/// Finishes the atom started at `at`: patches in its size.
fn end_atom(out: &mut [u8], at: usize) -> Result<()> {
    let size = u32::try_from(out.len() - at)
        .map_err(|_| Error::Unsupported("isomp4: metadata atom is larger than 4 GiB"))?;
    out[at..at + 4].copy_from_slice(&size.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use symphonia_core::io::MediaSourceStream;
    use symphonia_core::meta::{RawTag, StandardVisualKey};

    use super::*;
    use crate::atoms::{AtomIterator, IlstAtom};

    fn s(v: &str) -> Arc<String> {
        Arc::new(v.to_string())
    }

    fn read_back(ilst: &[u8]) -> (Vec<Tag>, Vec<Visual>) {
        let len = ilst.len() as u64;
        let mss = MediaSourceStream::new(Box::new(Cursor::new(ilst.to_vec())), Default::default());
        let mut it = AtomIterator::new(mss, Some(len));

        assert!(it.next_header().ok().flatten().is_some(), "ilst header should be read");
        let ilst = match it.read_atom::<IlstAtom>() {
            Ok(ilst) => ilst,
            Err(_) => panic!("ilst should parse"),
        };

        (ilst.metadata.media.tags, ilst.metadata.media.visuals)
    }

    fn std_tags(tags: &[Tag]) -> Vec<StandardTag> {
        tags.iter().filter_map(|t| t.std.clone()).collect()
    }

    fn std_tag(std: StandardTag) -> Tag {
        Tag::new_std(RawTag::new("test", "x"), std)
    }

    fn build(tags: &[Tag], visuals: &[Visual]) -> Vec<u8> {
        build_ilst(&[], tags, visuals).expect("serialize").expect("not empty")
    }

    fn visual(data: Vec<u8>) -> Visual {
        Visual {
            media_type: None,
            dimensions: None,
            color_mode: None,
            usage: Some(StandardVisualKey::FrontCover),
            tags: Vec::new(),
            data: data.into_boxed_slice(),
        }
    }

    #[test]
    fn standard_tags_round_trip_through_the_reader() {
        use StandardTag as S;

        let expected = vec![
            S::TrackTitle(s("Title")),
            S::Artist(s("Artist")),
            S::AlbumArtist(s("Album Artist")),
            S::Album(s("Album")),
            S::ReleaseDate(s("2024-05-06")),
            S::Genre(s("Genre")),
            S::TrackNumber(3),
            S::TrackTotal(12),
            S::DiscNumber(1),
            S::DiscTotal(2),
            S::CompilationFlag(true),
            S::Bpm(128),
            S::Comment(s("A comment")),
            S::Lyrics(s("la la la")),
            S::Composer(s("Composer")),
            S::Grouping(s("Group")),
            S::Copyright(s("(c) me")),
            S::Encoder(s("Aulos")),
            S::SortTrackTitle(s("sort title")),
            S::SortArtist(s("sort artist")),
            S::SortAlbum(s("sort album")),
            S::SortAlbumArtist(s("sort album artist")),
            S::SortComposer(s("sort composer")),
            S::MusicBrainzTrackId(s("11111111-1111-1111-1111-111111111111")),
            S::MusicBrainzAlbumId(s("22222222-2222-2222-2222-222222222222")),
            S::MusicBrainzArtistId(s("33333333-3333-3333-3333-333333333333")),
            S::MusicBrainzAlbumArtistId(s("44444444-4444-4444-4444-444444444444")),
            S::MusicBrainzReleaseGroupId(s("55555555-5555-5555-5555-555555555555")),
            S::MusicBrainzReleaseTrackId(s("66666666-6666-6666-6666-666666666666")),
            S::MusicBrainzReleaseType(s("album")),
            S::MusicBrainzReleaseStatus(s("official")),
            S::ReleaseCountry(s("XW")),
            S::IdentIsrc(s("USRC17607839")),
            S::IdentBarcode(s("0123456789012")),
            S::IdentCatalogNumber(s("CAT-1")),
            S::IdentAsin(s("B000000000")),
            S::Label(s("A Label")),
            S::Conductor(s("Conductor")),
            S::Lyricist(s("Lyricist")),
            S::Producer(s("Producer")),
            S::Remixer(s("Remixer")),
            S::Mood(s("Mellow")),
            S::Language(s("eng")),
            S::Script(s("Latn")),
            S::MediaFormat(s("CD")),
            S::DiscSubtitle(s("Disc subtitle")),
            S::TrackSubtitle(s("Subtitle")),
            S::OriginalReleaseDate(s("1999-01-01")),
            S::ReplayGainTrackGain(s("-6.5 dB")),
            S::ReplayGainAlbumPeak(s("0.98")),
            S::AcoustIdId(s("77777777-7777-7777-7777-777777777777")),
            S::MovementName(s("Allegro")),
            S::MovementNumber(2),
            S::MovementTotal(4),
            S::Work(s("Symphony No. 5")),
        ];

        let tags: Vec<Tag> = expected.iter().cloned().map(std_tag).collect();
        let (read, _) = read_back(&build(&tags, &[]));
        let read = std_tags(&read);

        for std in &expected {
            assert!(read.contains(std), "{std:?} did not round-trip, got {read:?}");
        }
        assert_eq!(read.len(), expected.len(), "unexpected extra tags: {read:?}");
    }

    #[test]
    fn recording_id_is_stored_as_the_track_id() {
        let tag = std_tag(StandardTag::MusicBrainzRecordingId(s("abc")));
        let (read, _) = read_back(&build(&[tag], &[]));

        assert_eq!(std_tags(&read), vec![StandardTag::MusicBrainzTrackId(s("abc"))]);
    }

    #[test]
    fn multiple_values_share_one_atom() {
        let tags = vec![
            std_tag(StandardTag::Artist(s("A"))),
            std_tag(StandardTag::Album(s("Album"))),
            std_tag(StandardTag::Artist(s("B"))),
        ];

        let ilst = build(&tags, &[]);
        let artist_atoms = ilst.windows(4).filter(|w| w == b"\xa9ART").count();
        assert_eq!(artist_atoms, 1);

        let (read, _) = read_back(&ilst);
        assert_eq!(
            std_tags(&read),
            vec![
                StandardTag::Artist(s("A")),
                StandardTag::Artist(s("B")),
                StandardTag::Album(s("Album"))
            ]
        );
    }

    #[test]
    fn identical_values_are_not_repeated() {
        let tags =
            vec![std_tag(StandardTag::Genre(s("Rock"))), std_tag(StandardTag::Genre(s("Rock")))];

        let (read, _) = read_back(&build(&tags, &[]));
        assert_eq!(std_tags(&read), vec![StandardTag::Genre(s("Rock"))]);
    }

    #[test]
    fn track_and_disc_pairs_are_binary() {
        let tags = vec![
            std_tag(StandardTag::TrackNumber(7)),
            std_tag(StandardTag::TrackTotal(9)),
            std_tag(StandardTag::DiscNumber(1)),
        ];

        let ilst = build(&tags, &[]);

        let trkn = ilst.windows(4).position(|w| w == b"trkn").unwrap();
        // trkn: size, 'trkn', data atom (16 byte header + 8 byte payload).
        assert_eq!(&ilst[trkn + 4 + 16..trkn + 4 + 16 + 8], &[0, 0, 0, 7, 0, 9, 0, 0]);

        let disk = ilst.windows(4).position(|w| w == b"disk").unwrap();
        assert_eq!(&ilst[disk + 4 + 16..disk + 4 + 16 + 6], &[0, 0, 0, 1, 0, 0]);

        let (read, _) = read_back(&ilst);
        assert_eq!(
            std_tags(&read),
            vec![
                StandardTag::TrackNumber(7),
                StandardTag::TrackTotal(9),
                StandardTag::DiscNumber(1)
            ]
        );
    }

    #[test]
    fn a_total_without_a_number_is_kept() {
        let (read, _) = read_back(&build(&[std_tag(StandardTag::TrackTotal(11))], &[]));
        assert_eq!(std_tags(&read), vec![StandardTag::TrackTotal(11)]);
    }

    #[test]
    fn genre_is_written_to_the_custom_genre_atom_only() {
        let ilst = build(&[std_tag(StandardTag::Genre(s("Rock")))], &[]);
        assert!(ilst.windows(4).any(|w| w == b"\xa9gen"));
        assert!(!ilst.windows(4).any(|w| w == b"gnre"));
    }

    #[test]
    fn composer_uses_the_wrt_atom_unless_read_from_com() {
        let ilst = build(&[std_tag(StandardTag::Composer(s("Bach")))], &[]);
        assert!(ilst.windows(4).any(|w| w == b"\xa9wrt"));

        let from_com =
            Tag::new_std(RawTag::new("\u{a9}com", "Bach"), StandardTag::Composer(s("Bach")));
        let ilst = build(&[from_com], &[]);
        assert!(ilst.windows(4).any(|w| w == b"\xa9com"));
        assert!(!ilst.windows(4).any(|w| w == b"\xa9wrt"));
    }

    #[test]
    fn tags_without_a_standard_tag_become_free_form_atoms() {
        let tags = vec![
            Tag::new(RawTag::new("MY FIELD", "one")),
            Tag::new(RawTag::new("MY FIELD", "two")),
            Tag::new(RawTag::new("com.apple.iTunes:iTunSMPB", " 00000000 00000840")),
            Tag::new(RawTag::new("org.example:Thing", "x")),
        ];

        let ilst = build(&tags, &[]);
        assert_eq!(ilst.windows(4).filter(|w| w == b"----").count(), 3);
        assert!(ilst.windows(16).any(|w| w == b"com.apple.iTunes"));
        assert!(ilst.windows(11).any(|w| w == b"org.example"));

        let (read, _) = read_back(&ilst);
        let keys: Vec<(&str, String)> =
            read.iter().map(|t| (t.raw.key.as_str(), t.raw.value.to_string())).collect();
        assert_eq!(
            keys,
            vec![
                ("com.apple.iTunes:MY FIELD", "one".to_string()),
                ("com.apple.iTunes:MY FIELD", "two".to_string()),
                ("com.apple.iTunes:iTunSMPB", " 00000000 00000840".to_string()),
                ("org.example:Thing", "x".to_string()),
            ]
        );
        assert!(read.iter().all(|t| t.std.is_none()));
    }

    #[test]
    fn known_raw_atoms_without_a_standard_tag_keep_their_atom() {
        let tags = vec![
            Tag::new(RawTag::new("pgap", RawValue::Boolean(true))),
            Tag::new(RawTag::new("apID", "me@example.com")),
            Tag::new(RawTag::new("cnID", RawValue::UnsignedInt(12345))),
        ];

        let ilst = build(&tags, &[]);
        assert!(!ilst.windows(4).any(|w| w == b"----"));

        let (read, _) = read_back(&ilst);
        assert_eq!(read[0].raw.key, "pgap");
        assert_eq!(read[0].raw.value, RawValue::SignedInt(1));
        assert_eq!(read[1].raw.key, "apID");
        assert_eq!(read[2].raw.key, "cnID");
        assert_eq!(read[2].raw.value, RawValue::SignedInt(12345));
    }

    #[test]
    fn mapped_free_form_names_keep_their_casing_and_aliases() {
        // Read from a file that stored the names in other casings: the atom is kept.
        let tags = vec![
            Tag::new_std(RawTag::new("com.apple.iTunes:artists", "A"), StandardTag::Artist(s("A"))),
            Tag::new_std(
                RawTag::new("com.apple.iTunes:musicbrainz album id", "m"),
                StandardTag::MusicBrainzAlbumId(s("m")),
            ),
        ];

        let ilst = build(&tags, &[]);
        assert!(ilst.windows(7).any(|w| w == b"artists"));
        assert!(ilst.windows(20).any(|w| w == b"musicbrainz album id"));
        assert!(!ilst.windows(4).any(|w| w == b"\xa9ART"));

        let (read, _) = read_back(&ilst);
        assert_eq!(
            std_tags(&read),
            vec![StandardTag::Artist(s("A")), StandardTag::MusicBrainzAlbumId(s("m"))]
        );
    }

    #[test]
    fn numeric_atoms_round_trip() {
        use StandardTag as S;

        let expected = vec![
            S::Bpm(200),
            S::TvEpisodeNumber(5),
            S::TvSeasonNumber(2),
            S::Rating(80 * 10_000),
            S::ContentAdvisory(ContentAdvisory::Explicit),
            S::PodcastFlag(true),
        ];

        let tags: Vec<Tag> = expected.iter().cloned().map(std_tag).collect();
        let (read, _) = read_back(&build(&tags, &[]));
        assert_eq!(std_tags(&read), expected);
    }

    #[test]
    fn media_kind_and_media_format_are_told_apart() {
        let kind = Tag::new_std(
            RawTag::new("stik", RawValue::SignedInt(2)),
            StandardTag::MediaFormat(s("Audio Book")),
        );
        let format = std_tag(StandardTag::MediaFormat(s("CD")));

        let ilst = build(&[kind, format], &[]);
        assert!(ilst.windows(4).any(|w| w == b"stik"));

        let (read, _) = read_back(&ilst);
        assert_eq!(
            std_tags(&read),
            vec![StandardTag::MediaFormat(s("Audio Book")), StandardTag::MediaFormat(s("CD"))]
        );
    }

    #[test]
    fn covers_become_one_data_atom_each() {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.extend_from_slice(&[0; 16]);
        let mut png = vec![0x89, b'P', b'N', b'G'];
        png.extend_from_slice(&[0; 16]);

        let ilst = build(&[], &[visual(jpeg.clone()), visual(png.clone())]);
        let (_, visuals) = read_back(&ilst);

        assert_eq!(visuals.len(), 2);
        assert_eq!(visuals[0].data.as_ref(), jpeg.as_slice());
        assert_eq!(visuals[1].data.as_ref(), png.as_slice());

        // Data types 13 (JPEG) and 14 (PNG) in the data atom flags.
        let types: Vec<u8> = ilst.windows(8).filter(|w| &w[..4] == b"data").map(|w| w[7]).collect();
        assert_eq!(types, vec![13, 14]);
    }

    #[test]
    fn unknown_cover_formats_are_rejected() {
        let err = build_ilst(&[], &[], &[visual(b"GIF89a....".to_vec())]).err();
        assert!(err.is_some());
    }

    #[test]
    fn nothing_to_write_yields_no_ilst() {
        assert!(build_ilst(&[], &[], &[]).unwrap().is_none());
    }

    #[test]
    fn unmanaged_atoms_are_preserved_and_managed_ones_replaced() {
        fn atom(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(fourcc);
            out.extend_from_slice(body);
            out
        }

        let mut existing = Vec::new();
        existing
            .extend(atom(b"\xa9nam", &atom(b"data", &[0, 0, 0, 1, 0, 0, 0, 0, b'o', b'l', b'd'])));
        existing.extend(atom(b"hdvd", &atom(b"data", &[0, 0, 0, 21, 0, 0, 0, 0, 1])));
        existing.extend(atom(b"zzzz", b"opaque"));
        // The last atom extends to the end of the list.
        let mut last = 0u32.to_be_bytes().to_vec();
        last.extend_from_slice(b"yyyy");
        last.extend_from_slice(b"tail");
        existing.extend(last);

        let ilst = build_ilst(&existing, &[std_tag(StandardTag::TrackTitle(s("new")))], &[])
            .unwrap()
            .unwrap();

        assert!(ilst.windows(3).any(|w| w == b"new"));
        assert!(!ilst.windows(3).any(|w| w == b"old"));
        assert!(ilst.windows(4).any(|w| w == b"hdvd"));
        assert!(ilst.windows(6).any(|w| w == b"opaque"));
        // The size-0 atom got an explicit size (8 + 4).
        let at = ilst.windows(4).position(|w| w == b"yyyy").unwrap();
        assert_eq!(&ilst[at - 4..at], &12u32.to_be_bytes());

        // Only unmanaged atoms: still an ilst.
        let only = build_ilst(&existing, &[], &[]).unwrap().unwrap();
        assert!(!only.windows(3).any(|w| w == b"old"));
        assert!(only.windows(4).any(|w| w == b"zzzz"));
    }

    #[test]
    fn hdlr_is_a_well_formed_mdir_handler() {
        let hdlr = mdir_hdlr_box();
        assert_eq!(hdlr.len(), 33);
        assert_eq!(u32::from_be_bytes([hdlr[0], hdlr[1], hdlr[2], hdlr[3]]), 33);
        assert_eq!(&hdlr[4..8], b"hdlr");
        assert_eq!(&hdlr[16..20], b"mdir");
    }
}
