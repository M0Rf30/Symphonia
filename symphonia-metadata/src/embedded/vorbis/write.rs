// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Vorbis Comment writing.
//!
//! The serializer is the inverse of [`read_vorbis_comment`](super::read_vorbis_comment): a [`Tag`]
//! with a standard tag is written under the key that the reader maps to that standard tag (the
//! canonical key, where the reader accepts several, is the one MusicBrainz Picard writes), and a
//! [`Tag`] without one is written under its raw key as a free-form field. Visuals are written as
//! base64 encoded FLAC picture blocks in `METADATA_BLOCK_PICTURE` comments.

use std::borrow::Cow;
use std::collections::HashSet;

use symphonia_core::errors::{Error, Result};
use symphonia_core::meta::{RawValue, StandardTag, Tag, Visual};

use crate::embedded::flac::write::picture_block;
use crate::embedded::vorbis::try_parse_chapter_info_key;
use crate::utils::base64;

/// Options for how tags are written to a Vorbis comment.
#[derive(Clone, Debug, Default)]
pub struct CommentOptions {
    /// The email of the rating's owner. A rating is written as `RATING:<email>` instead of
    /// `RATING` if set. A rating tag that was read under a `RATING:<other>` key keeps that key.
    pub rating_email: Option<String>,
    /// Write ratings as a decimal fraction from 0.0 to 1.0 (`RATING=0.8`) instead of an integer
    /// from 0 to 100 (`RATING=80`).
    pub rating_fraction: bool,
}

/// Gets the key and value of a rating comment.
fn rating_field(tag: &Tag, ppm: u32, options: &CommentOptions) -> (String, String) {
    let raw_key = tag.raw.key.as_str();

    let key = if raw_key.len() > 7 && raw_key.as_bytes()[..7].eq_ignore_ascii_case(b"rating:") {
        raw_key.chars().filter(|c| matches!(c, '\u{20}'..='\u{7d}') && *c != '=').collect()
    }
    else if let Some(email) = options.rating_email.as_deref() {
        format!("RATING:{email}")
    }
    else {
        "RATING".to_string()
    };

    let value = if options.rating_fraction {
        let mut s = format!("{:.6}", f64::from(ppm) / 1_000_000.0);
        while s.ends_with('0') && !s.ends_with(".0") {
            s.pop();
        }
        s
    }
    else {
        (ppm / 10000).to_string()
    };

    (key, value)
}

/// The parts of an existing comment header that the tag model cannot represent, and that must
/// survive a rewrite of the tags.
#[derive(Clone, Debug, Default)]
pub struct ExistingComments {
    /// The vendor string, as is.
    pub vendor: Vec<u8>,
    /// Comments that are not tags (`CHAPTERxxx` chapter information), as is.
    pub preserved: Vec<Vec<u8>>,
    /// Any bytes that follow the comment list. For a Vorbis comment header this is the framing
    /// bit, for an Opus comment header it may be arbitrary padding that must be preserved
    /// (RFC 7845, section 5.2).
    pub trailing: Vec<u8>,
}

/// Parses an existing comment header body (starting at the vendor string length) for the parts
/// that must be preserved. Returns `None` if the body is malformed.
pub fn parse_existing(buf: &[u8]) -> Option<ExistingComments> {
    fn take<'a>(buf: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
        if buf.len() < len {
            return None;
        }
        let (head, tail) = buf.split_at(len);
        *buf = tail;
        Some(head)
    }

    fn take_u32(buf: &mut &[u8]) -> Option<u32> {
        take(buf, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    let mut rem = buf;

    let vendor_len = take_u32(&mut rem)? as usize;
    let vendor = take(&mut rem, vendor_len)?.to_vec();

    let num_comments = take_u32(&mut rem)?;

    let mut preserved = Vec::new();

    for _ in 0..num_comments {
        let len = take_u32(&mut rem)? as usize;
        let comment = take(&mut rem, len)?;

        let is_chapter = comment
            .iter()
            .position(|&b| b == b'=')
            .and_then(|eq| std::str::from_utf8(&comment[..eq]).ok())
            .is_some_and(|key| try_parse_chapter_info_key(key).is_some());

        if is_chapter {
            preserved.push(comment.to_vec());
        }
    }

    Some(ExistingComments { vendor, preserved, trailing: rem.to_vec() })
}

/// Serializes a Vorbis comment body: the vendor string, followed by a comment for every tag, for
/// every visual in `embedded_visuals` (as a `METADATA_BLOCK_PICTURE` comment), and every comment
/// in `preserved`.
///
/// The framing bit of a Vorbis comment *header* is not part of the body.
pub fn comment_body(
    vendor: &[u8],
    tags: &[Tag],
    embedded_visuals: &[Visual],
    preserved: &[Vec<u8>],
) -> Result<Vec<u8>> {
    comment_body_with(vendor, tags, embedded_visuals, preserved, &CommentOptions::default())
}

/// Like [`comment_body`], with options for how tags are written.
pub fn comment_body_with(
    vendor: &[u8],
    tags: &[Tag],
    embedded_visuals: &[Visual],
    preserved: &[Vec<u8>],
    options: &CommentOptions,
) -> Result<Vec<u8>> {
    let mut comments = encode_tags_with(tags, options);

    for visual in embedded_visuals {
        let mut comment = b"METADATA_BLOCK_PICTURE=".to_vec();
        comment.extend_from_slice(base64::encode(&picture_block(visual)?).as_bytes());
        comments.push(comment);
    }

    comments.extend(preserved.iter().cloned());

    let too_large = || Error::Unsupported("meta (vorbis): comment header exceeds 4 GiB");

    let mut out =
        Vec::with_capacity(8 + vendor.len() + comments.iter().map(|c| 4 + c.len()).sum::<usize>());

    out.extend_from_slice(&u32::try_from(vendor.len()).map_err(|_| too_large())?.to_le_bytes());
    out.extend_from_slice(vendor);
    out.extend_from_slice(&u32::try_from(comments.len()).map_err(|_| too_large())?.to_le_bytes());

    for comment in &comments {
        out.extend_from_slice(
            &u32::try_from(comment.len()).map_err(|_| too_large())?.to_le_bytes(),
        );
        out.extend_from_slice(comment);
    }

    Ok(out)
}

/// Encodes tags as `KEY=value` comments. Tags that have no value, and exact duplicates, are
/// skipped.
pub fn encode_tags(tags: &[Tag]) -> Vec<Vec<u8>> {
    encode_tags_with(tags, &CommentOptions::default())
}

/// Like [`encode_tags`], with options for how tags are written.
pub fn encode_tags_with(tags: &[Tag], options: &CommentOptions) -> Vec<Vec<u8>> {
    let mut comments = Vec::with_capacity(tags.len());
    let mut seen = HashSet::new();

    let mut push = |key: &str, value: &str| {
        if value.is_empty() {
            return;
        }

        let mut comment = Vec::with_capacity(key.len() + 1 + value.len());
        comment.extend_from_slice(key.as_bytes());
        comment.push(b'=');
        comment.extend_from_slice(value.as_bytes());

        if seen.insert(comment.clone()) {
            comments.push(comment);
        }
    };

    for tag in tags {
        if let Some(StandardTag::Rating(ppm)) = tag.std.as_ref() {
            let (key, value) = rating_field(tag, *ppm, options);
            push(&key, &value);
            continue;
        }

        if let Some((key, value)) = tag.std.as_ref().and_then(std_field) {
            push(key, &value);
            continue;
        }

        // Free-form field.
        let Some(key) = free_form_key(&tag.raw.key)
        else {
            continue;
        };

        match &tag.raw.value {
            RawValue::String(value) => push(&key, value),
            RawValue::StringList(values) => values.iter().for_each(|value| push(&key, value)),
            RawValue::Boolean(value) => push(&key, &value.to_string()),
            RawValue::Float(value) => push(&key, &value.to_string()),
            RawValue::SignedInt(value) => push(&key, &value.to_string()),
            RawValue::UnsignedInt(value) => push(&key, &value.to_string()),
            // These cannot be represented in a text comment.
            RawValue::Binary(_) | RawValue::Flag => (),
            _ => (),
        }
    }

    comments
}

/// Gets the key a free-form tag is written under: the raw key, upper-cased, restricted to the
/// printable ASCII characters (without `=`) that are valid in a comment key. Keys that are
/// handled otherwise (pictures) yield `None`, as do empty keys.
fn free_form_key(raw: &str) -> Option<String> {
    let raw = raw.strip_prefix("TXXX:").unwrap_or(raw);

    let key: String = raw
        .chars()
        .filter(|c| matches!(c, '\u{20}'..='\u{7d}') && *c != '=')
        .map(|c| c.to_ascii_uppercase())
        .collect();

    if key.is_empty() || key == "METADATA_BLOCK_PICTURE" || key == "COVERART" {
        None
    }
    else {
        Some(key)
    }
}

/// Gets the key and value of the comment that the reader maps to the standard tag, or `None` if
/// the reader does not map any comment to it.
fn std_field(std: &StandardTag) -> Option<(&'static str, Cow<'_, str>)> {
    use StandardTag::*;
    fn text<'a>(key: &'static str, v: &'a str) -> Option<(&'static str, Cow<'a, str>)> {
        Some((key, Cow::Borrowed(v)))
    }

    fn num<'a>(key: &'static str, v: u64) -> Option<(&'static str, Cow<'a, str>)> {
        Some((key, Cow::Owned(v.to_string())))
    }

    match std {
        AccurateRipCount(v) => text("ACCURATERIPCOUNT", v),
        AccurateRipCountAllOffsets(v) => text("ACCURATERIPCOUNTALLOFFSETS", v),
        AccurateRipCountWithOffset(v) => text("ACCURATERIPCOUNTWITHOFFSET", v),
        AccurateRipCrc(v) => text("ACCURATERIPCRC", v),
        AccurateRipDiscId(v) => text("ACCURATERIPDISCID", v),
        AccurateRipId(v) => text("ACCURATERIPID", v),
        AccurateRipOffset(v) => text("ACCURATERIPOFFSET", v),
        AccurateRipResult(v) => text("ACCURATERIPRESULT", v),
        AccurateRipTotal(v) => text("ACCURATERIPTOTAL", v),
        AcoustIdFingerprint(v) => text("ACOUSTID_FINGERPRINT", v),
        AcoustIdId(v) => text("ACOUSTID_ID", v),
        Album(v) => text("ALBUM", v),
        AlbumArtist(v) => text("ALBUMARTIST", v),
        Arranger(v) => text("ARRANGER", v),
        Artist(v) => text("ARTIST", v),
        Bpm(v) => num("BPM", *v),
        CdToc(v) => text("CDTOC", v),
        Comment(v) => text("COMMENT", v),
        CompilationFlag(v) => text("COMPILATION", if *v { "1" } else { "0" }),
        Composer(v) => text("COMPOSER", v),
        Conductor(v) => text("CONDUCTOR", v),
        Copyright(v) => text("COPYRIGHT", v),
        CueToolsDbDiscConfidence(v) => text("CTDBDISCCONFIDENCE", v),
        CueToolsDbTrackConfidence(v) => text("CTDBTRACKCONFIDENCE", v),
        Description(v) => text("DESCRIPTION", v),
        DiscNumber(v) => num("DISCNUMBER", *v),
        DiscSubtitle(v) => text("DISCSUBTITLE", v),
        DiscTotal(v) => num("DISCTOTAL", *v),
        EncodedBy(v) => text("ENCODEDBY", v),
        Encoder(v) => text("ENCODER", v),
        EncoderSettings(v) => text("ENCODERSETTINGS", v),
        Engineer(v) => text("ENGINEER", v),
        Ensemble(v) => text("ENSEMBLE", v),
        Genre(v) => text("GENRE", v),
        Grouping(v) => text("GROUPING", v),
        IdentAsin(v) => text("ASIN", v),
        IdentBarcode(v) => text("BARCODE", v),
        IdentCatalogNumber(v) => text("CATALOGNUMBER", v),
        IdentEanUpn(v) => text("EAN/UPN", v),
        IdentIsrc(v) => text("ISRC", v),
        IdentPn(v) => text("PRODUCTNUMBER", v),
        IdentUpc(v) => text("UPC", v),
        InitialKey(v) => text("KEY", v),
        Label(v) => text("LABEL", v),
        Language(v) => text("LANGUAGE", v),
        License(v) => text("LICENSE", v),
        Lyricist(v) => text("LYRICIST", v),
        Lyrics(v) => text("LYRICS", v),
        MediaFormat(v) => text("MEDIA", v),
        MixDj(v) => text("DJMIXER", v),
        MixEngineer(v) => text("MIXER", v),
        Mood(v) => text("MOOD", v),
        MovementName(v) => text("MOVEMENTNAME", v),
        MovementNumber(v) => num("MOVEMENT", *v),
        MovementTotal(v) => num("MOVEMENTTOTAL", *v),
        MusicBrainzAlbumArtistId(v) => text("MUSICBRAINZ_ALBUMARTISTID", v),
        MusicBrainzAlbumId(v) => text("MUSICBRAINZ_ALBUMID", v),
        MusicBrainzArtistId(v) => text("MUSICBRAINZ_ARTISTID", v),
        MusicBrainzDiscId(v) => text("MUSICBRAINZ_DISCID", v),
        MusicBrainzOriginalAlbumId(v) => text("MUSICBRAINZ_ORIGINALALBUMID", v),
        MusicBrainzOriginalArtistId(v) => text("MUSICBRAINZ_ORIGINALARTISTID", v),
        // In Vorbis comments (and as Picard writes them) the recording ID is stored in
        // MUSICBRAINZ_TRACKID, so the recording ID and the track ID share a key.
        MusicBrainzRecordingId(v) => text("MUSICBRAINZ_TRACKID", v),
        MusicBrainzReleaseGroupId(v) => text("MUSICBRAINZ_RELEASEGROUPID", v),
        MusicBrainzReleaseStatus(v) => text("RELEASESTATUS", v),
        MusicBrainzReleaseTrackId(v) => text("MUSICBRAINZ_RELEASETRACKID", v),
        MusicBrainzReleaseType(v) => text("RELEASETYPE", v),
        MusicBrainzTrackId(v) => text("MUSICBRAINZ_TRACKID", v),
        MusicBrainzWorkId(v) => text("MUSICBRAINZ_WORKID", v),
        Opus(v) => text("OPUS", v),
        OriginalReleaseDate(v) => text("ORIGINALDATE", v),
        OriginalReleaseYear(v) => num("ORIGINALYEAR", u64::from(*v)),
        Part(v) => text("PART", v),
        PartNumber(v) => num("PARTNUMBER", *v),
        Performer(v) => text("PERFORMER", v),
        Producer(v) => text("PRODUCER", v),
        // Rating is stored in PPM, the comment is on a 0 - 100 scale.
        Rating(v) => num("RATING", u64::from(*v / 10000)),
        RecordingDate(v) => text("DATE", v),
        RecordingYear(v) => num("YEAR", u64::from(*v)),
        ReleaseCountry(v) => text("RELEASECOUNTRY", v),
        Remixer(v) => text("REMIXER", v),
        ReplayGainAlbumGain(v) => text("REPLAYGAIN_ALBUM_GAIN", v),
        ReplayGainAlbumPeak(v) => text("REPLAYGAIN_ALBUM_PEAK", v),
        ReplayGainReferenceLoudness(v) => text("REPLAYGAIN_REFERENCE_LOUDNESS", v),
        ReplayGainTrackGain(v) => text("REPLAYGAIN_TRACK_GAIN", v),
        ReplayGainTrackPeak(v) => text("REPLAYGAIN_TRACK_PEAK", v),
        Script(v) => text("SCRIPT", v),
        SortAlbum(v) => text("ALBUMSORT", v),
        SortAlbumArtist(v) => text("ALBUMARTISTSORT", v),
        SortArtist(v) => text("ARTISTSORT", v),
        SortComposer(v) => text("COMPOSERSORT", v),
        SortTrackTitle(v) => text("TITLESORT", v),
        TrackNumber(v) => num("TRACKNUMBER", *v),
        TrackSubtitle(v) => text("SUBTITLE", v),
        TrackTitle(v) => text("TITLE", v),
        TrackTotal(v) => num("TRACKTOTAL", *v),
        Version(v) => text("VERSION", v),
        Work(v) => text("WORK", v),
        Writer(v) => text("WRITER", v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use symphonia_core::io::{BufReader, ReadBytes};
    use symphonia_core::meta::{MetadataBuilder, MetadataSideData, RawTag, StandardTag, Tag};

    use super::*;
    use crate::embedded::vorbis::read_vorbis_comment;

    fn s(v: &str) -> Arc<String> {
        Arc::new(v.to_string())
    }

    fn read_back(body: &[u8]) -> Vec<Tag> {
        let mut builder =
            MetadataBuilder::new(crate::embedded::vorbis::VORBIS_COMMENT_METADATA_INFO);
        let mut side_data: Vec<MetadataSideData> = Vec::new();
        read_vorbis_comment(&mut BufReader::new(body), &mut builder, &mut side_data).unwrap();
        builder.build().media.tags
    }

    #[test]
    fn verify_round_trip() {
        let tags = vec![
            Tag::new_from_parts(
                "TITLE",
                "Zażółć gęślą jaźń ♪",
                Some(StandardTag::TrackTitle(s("Zażółć gęślą jaźń ♪"))),
            ),
            Tag::new_from_parts("ARTIST", "A", Some(StandardTag::Artist(s("A")))),
            Tag::new_from_parts("ARTIST", "B", Some(StandardTag::Artist(s("B")))),
            Tag::new_from_parts("TRACKNUMBER", "3/12", Some(StandardTag::TrackNumber(3))),
            Tag::new_from_parts("TRACKNUMBER", "3/12", Some(StandardTag::TrackTotal(12))),
            Tag::new_from_parts("DISCNUMBER", "1/2", Some(StandardTag::DiscNumber(1))),
            Tag::new_from_parts("DISCNUMBER", "1/2", Some(StandardTag::DiscTotal(2))),
            Tag::new_from_parts("TXXX:My Key", "free", None),
            Tag::new_from_parts("RELEASECOUNTRY", "IT", Some(StandardTag::ReleaseCountry(s("IT")))),
            Tag::new_from_parts("x", "", None),
        ];

        let body = comment_body(b"vendor", &tags, &[], &[]).unwrap();

        let mut reader = BufReader::new(&body);
        assert_eq!(reader.read_u32().unwrap(), 6);

        let read = read_back(&body);

        let find = |std: StandardTag| read.iter().filter(|t| t.std.as_ref() == Some(&std)).count();

        assert_eq!(find(StandardTag::Artist(s("A"))), 1);
        assert_eq!(find(StandardTag::Artist(s("B"))), 1);
        assert_eq!(find(StandardTag::TrackNumber(3)), 1);
        assert_eq!(find(StandardTag::TrackTotal(12)), 1);
        assert_eq!(find(StandardTag::DiscNumber(1)), 1);
        assert_eq!(find(StandardTag::DiscTotal(2)), 1);
        assert_eq!(find(StandardTag::ReleaseCountry(s("IT"))), 1);
        assert_eq!(find(StandardTag::TrackTitle(s("Zażółć gęślą jaźń ♪"))), 1);
        assert!(read.iter().any(|t| t.raw.key == "MY KEY" && t.std.is_none()));
        assert_eq!(read.len(), 9);
    }

    #[test]
    fn verify_recording_and_track_id_share_a_comment() {
        let id = "b9ad642e-b012-41c7-b72a-42cf4911f9ff";
        let tags = vec![
            Tag::new_std(RawTag::new("A", id), StandardTag::MusicBrainzRecordingId(s(id))),
            Tag::new_std(RawTag::new("B", id), StandardTag::MusicBrainzTrackId(s(id))),
        ];
        assert_eq!(encode_tags(&tags).len(), 1);
    }

    #[test]
    fn verify_parse_existing() {
        let chapter = b"CHAPTER001=00:00:00.000".to_vec();
        let mut body = comment_body(
            b"vend",
            &[Tag::new_from_parts("TITLE", "t", None)],
            &[],
            std::slice::from_ref(&chapter),
        )
        .unwrap();
        body.push(1);

        let existing = parse_existing(&body).unwrap();
        assert_eq!(existing.vendor, b"vend");
        assert_eq!(existing.preserved, vec![chapter]);
        assert_eq!(existing.trailing, vec![1]);

        assert!(parse_existing(&body[..6]).is_none());
    }
}
