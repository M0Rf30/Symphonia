// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ID3v1 metadata reader.

use std::io::{Seek, SeekFrom};
use std::sync::Arc;

use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::formats::probe::{
    Anchors, ProbeMetadataData, ProbeableMetadata, Score, Scoreable,
};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes, ScopedStream};
use symphonia_core::meta::well_known::METADATA_ID_ID3V1;
use symphonia_core::meta::{
    MetadataBuffer, MetadataBuilder, MetadataInfo, MetadataOptions, MetadataReader, StandardTag,
    Tag,
};
use symphonia_core::support_metadata;

use crate::utils::id3v1::{decode_windows_1252_byte, get_genre_name};

fn read_id3v1<B: ReadBytes>(
    reader: &mut B,
    enhanced: Option<&[u8; ID3V1_ENHANCED_LEN]>,
    builder: &mut MetadataBuilder,
) -> Result<()> {
    // Read the "TAG" header.
    let marker = reader.read_triple_bytes()?;

    if marker != *b"TAG" {
        return unsupported_error("id3v1: Not an ID3v1 tag");
    }

    let mut buf = [0u8; 125];
    reader.read_buf_exact(&mut buf)?;

    // The enhanced tag extends the title, artist, and album fields to 60 characters each, and
    // adds a free-text genre. Prefer its values over the ID3v1 values when present.
    // Layout: "TAG+", title (60), artist (60), album (60), speed (1), genre (30), start time (6),
    // end time (6).
    let ext_title = enhanced.and_then(|e| decode_iso8859_buf(&e[4..64]));
    let ext_artist = enhanced.and_then(|e| decode_iso8859_buf(&e[64..124]));
    let ext_album = enhanced.and_then(|e| decode_iso8859_buf(&e[124..184]));
    let ext_genre = enhanced.and_then(|e| decode_iso8859_buf(&e[185..215]));

    if let Some(title) = ext_title.or_else(|| decode_iso8859_buf(&buf[0..30])) {
        let tag = Tag::new_from_parts("TITLE", title.clone(), Some(StandardTag::TrackTitle(title)));
        builder.add_tag(tag);
    }

    if let Some(artist) = ext_artist.or_else(|| decode_iso8859_buf(&buf[30..60])) {
        let tag = Tag::new_from_parts("ARTIST", artist.clone(), Some(StandardTag::Artist(artist)));
        builder.add_tag(tag);
    }

    if let Some(album) = ext_album.or_else(|| decode_iso8859_buf(&buf[60..90])) {
        let tag = Tag::new_from_parts("ALBUM", album.clone(), Some(StandardTag::Album(album)));
        builder.add_tag(tag);
    }

    if let Some(year) = decode_iso8859_buf(&buf[90..94]) {
        let tag = Tag::new_from_parts(
            "YEAR",
            year.clone(),
            year.parse::<u16>().ok().map(StandardTag::RecordingYear),
        );
        builder.add_tag(tag);
    }

    // If the second-last byte of the comment field is 0 (indicating the remaining characters are
    // also 0), then the last byte of the comment field is the track number (ID3v1.1). A track
    // number of 0 means the track number is unset, and must not be reported.
    let comment = if buf[122] == 0 {
        let track = u64::from(buf[123]);

        if track != 0 {
            builder.add_tag(Tag::new_from_parts(
                "TRACK",
                track,
                Some(StandardTag::TrackNumber(track)),
            ));
        }

        decode_iso8859_buf(&buf[94..122])
    }
    else {
        decode_iso8859_buf(&buf[94..124])
    };

    if let Some(comment) = comment {
        let tag =
            Tag::new_from_parts("COMMENT", comment.clone(), Some(StandardTag::Comment(comment)));
        builder.add_tag(tag);
    }

    // Use the free-text genre of the enhanced tag if present. Otherwise, convert the genre index
    // to an actual genre name using the GENRES lookup table.
    if let Some(genre) = ext_genre.or_else(|| get_genre_name(buf[124]).map(Arc::new)) {
        let tag = Tag::new_from_parts("GENRE", genre.clone(), Some(StandardTag::Genre(genre)));
        builder.add_tag(tag);
    }

    Ok(())
}

fn decode_iso8859_buf(buf: &[u8]) -> Option<Arc<String>> {
    // Decode a null-terminated string. ID3v1 does not specify an encoding, so assume
    // ISO-8859-1 (with the C1 range interpreted as Windows-1252, as it is in practice) such that
    // all characters codes are valid.
    let text = buf
        .iter()
        .copied()
        .take_while(|&c| c != 0)
        .map(decode_windows_1252_byte)
        .collect::<String>();

    // Trailing spaces are padding.
    let text = text.trim_end_matches(' ');

    // Do not return an empty string.
    if !text.is_empty() { Some(Arc::new(text.to_string())) } else { None }
}

/// The length of the ID3v1 "enhanced" tag ("TAG+") that may immediately precede an ID3v1 tag.
const ID3V1_ENHANCED_LEN: usize = 227;

/// Look for an ID3v1 enhanced tag ("TAG+") in the `ID3V1_ENHANCED_LEN` bytes immediately before
/// the current position, and return it if present. The position of the stream is restored.
///
/// Returns `None` if the stream is not seekable, there is not enough data before the ID3v1 tag, or
/// no enhanced tag is present.
fn read_enhanced_tag(mss: &mut MediaSourceStream<'_>) -> Result<Option<[u8; ID3V1_ENHANCED_LEN]>> {
    if !mss.is_seekable() {
        return Ok(None);
    }

    let pos = mss.pos();

    let Some(start) = pos.checked_sub(ID3V1_ENHANCED_LEN as u64)
    else {
        return Ok(None);
    };

    let mut buf = [0u8; ID3V1_ENHANCED_LEN];

    let found = mss.seek(SeekFrom::Start(start)).is_ok() && mss.read_buf_exact(&mut buf).is_ok();

    // Restore the position of the stream.
    mss.seek(SeekFrom::Start(pos))?;

    Ok((found && buf[..4] == *b"TAG+").then_some(buf))
}

const ID3V1_METADATA_INFO: MetadataInfo =
    MetadataInfo { metadata: METADATA_ID_ID3V1, short_name: "id3v1", long_name: "ID3v1" };

/// ID3v1 tag reader.
pub struct Id3v1Reader<'s> {
    reader: MediaSourceStream<'s>,
}

impl<'s> Id3v1Reader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, _opts: MetadataOptions) -> Result<Self> {
        Ok(Self { reader: mss })
    }
}

impl Scoreable for Id3v1Reader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableMetadata<'_> for Id3v1Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: MetadataOptions,
    ) -> Result<Box<dyn MetadataReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(Id3v1Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeMetadataData] {
        &[support_metadata!(ID3V1_METADATA_INFO, &[], &[], &[b"TAG"], Anchors::Exclusive(&[128]))]
    }
}

impl MetadataReader for Id3v1Reader<'_> {
    fn metadata_info(&self) -> &MetadataInfo {
        &ID3V1_METADATA_INFO
    }

    fn read_all(&mut self) -> Result<MetadataBuffer> {
        let mut builder = MetadataBuilder::new(ID3V1_METADATA_INFO);
        let enhanced = read_enhanced_tag(&mut self.reader)?;
        read_id3v1(&mut self.reader, enhanced.as_ref(), &mut builder)?;
        Ok(MetadataBuffer { revision: builder.build(), side_data: Vec::new() })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

#[cfg(feature = "write")]
pub mod write {
    //! Serialization of an ID3v1.1 tag from tags.

    use symphonia_core::meta::{StandardTag, Tag};

    use crate::utils::id3v1::{encode_windows_1252_char, get_genre_index};

    /// The length of an ID3v1 tag in bytes.
    pub const ID3V1_TAG_LEN: usize = 128;

    /// The length of the ID3v1 "enhanced" tag ("TAG+") that may immediately precede an ID3v1 tag.
    pub const ID3V1_ENHANCED_TAG_LEN: usize = super::ID3V1_ENHANCED_LEN;

    /// Returns the start of an enhanced ID3v1 tag ("TAG+") that ends at `tag_start`, if there is
    /// one beginning at or after `floor`.
    ///
    /// # Errors
    ///
    /// Any I/O error of `src`.
    pub fn find_enhanced_start<R: std::io::Read + std::io::Seek>(
        src: &mut R,
        tag_start: u64,
        floor: u64,
    ) -> std::io::Result<Option<u64>> {
        let Some(start) =
            tag_start.checked_sub(ID3V1_ENHANCED_TAG_LEN as u64).filter(|&s| s >= floor)
        else {
            return Ok(None);
        };

        let mut marker = [0u8; 4];
        src.seek(std::io::SeekFrom::Start(start))?;
        src.read_exact(&mut marker)?;

        Ok((&marker == b"TAG+").then_some(start))
    }

    /// Write `text` into `field`: encoded as Windows-1252 (characters that cannot be represented
    /// become `?`), truncated to the size of the field and padded with NULs.
    fn put_text(field: &mut [u8], text: &str) {
        let encoded = text
            .chars()
            .filter(|&c| c != '\0')
            .map(|c| encode_windows_1252_char(c).unwrap_or(b'?'));

        for (dst, src) in field.iter_mut().zip(encoded) {
            *dst = src;
        }
    }

    /// Serialize the tags into a 128 byte ID3v1.1 tag.
    ///
    /// Only the tags that ID3v1 can hold are written: the title, artist, album, year, comment,
    /// track number (ID3v1.1: a comment is then limited to 28 characters) and genre. Fields are
    /// truncated to their size. The genre is written as the index of the ID3v1 genre of the same
    /// name, or as unset (255) when the genre is not an ID3v1 genre. The first tag of each kind is
    /// used.
    pub fn serialize(tags: &[Tag]) -> [u8; ID3V1_TAG_LEN] {
        let mut buf = [0u8; ID3V1_TAG_LEN];
        buf[..3].copy_from_slice(b"TAG");
        // Unset genre.
        buf[127] = 255;

        let mut year: Option<String> = None;
        let mut track = None;
        let mut comment = None;

        for tag in tags {
            match &tag.std {
                Some(StandardTag::TrackTitle(v)) if buf[3] == 0 => put_text(&mut buf[3..33], v),
                Some(StandardTag::Artist(v)) if buf[33] == 0 => put_text(&mut buf[33..63], v),
                Some(StandardTag::Album(v)) if buf[63] == 0 => put_text(&mut buf[63..93], v),
                Some(StandardTag::RecordingDate(v)) => {
                    // A recording date is preferred to a year only.
                    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
                    if digits.len() == 4 && year.is_none() {
                        year = Some(digits);
                    }
                }
                Some(StandardTag::RecordingYear(v)) if year.is_none() => {
                    year = Some(format!("{v:04}"));
                }
                Some(StandardTag::Comment(v)) if comment.is_none() => comment = Some(v.clone()),
                Some(StandardTag::TrackNumber(v)) if track.is_none() => {
                    track = u8::try_from(*v).ok().filter(|&t| t != 0);
                }
                Some(StandardTag::Genre(v)) if buf[127] == 255 => {
                    if let Some(index) = get_genre_index(v) {
                        buf[127] = index;
                    }
                }
                _ => (),
            }
        }

        if let Some(year) = year.filter(|y| y.len() == 4) {
            buf[93..97].copy_from_slice(year.as_bytes());
        }

        // In ID3v1.1 the last byte of the comment is the track number, and the byte before it is 0.
        match (comment, track) {
            (comment, Some(track)) => {
                if let Some(comment) = comment {
                    put_text(&mut buf[97..125], &comment);
                }
                buf[126] = track;
            }
            (Some(comment), None) => put_text(&mut buf[97..127], &comment),
            (None, None) => (),
        }

        buf
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use symphonia_core::io::BufReader;
        use symphonia_core::meta::{MetadataBuilder, StandardTag, Tag};

        use super::serialize;
        use crate::id3v1::{ID3V1_METADATA_INFO, read_id3v1};

        fn std(tag: StandardTag) -> Tag {
            Tag::new_std(symphonia_core::meta::RawTag::new("", ""), tag)
        }

        fn s(v: &str) -> Arc<String> {
            Arc::new(v.to_string())
        }

        fn read(buf: &[u8; 128]) -> Vec<StandardTag> {
            let mut builder = MetadataBuilder::new(ID3V1_METADATA_INFO);
            read_id3v1(&mut BufReader::new(buf), None, &mut builder).unwrap();
            builder.build().media.tags.into_iter().filter_map(|t| t.std).collect()
        }

        #[test]
        fn verify_round_trip_with_track_and_genre() {
            let tags = [
                std(StandardTag::TrackTitle(s("Caf\u{e9} \u{20ac}uro"))),
                std(StandardTag::Artist(s("Artist"))),
                std(StandardTag::Album(s("Album"))),
                std(StandardTag::RecordingDate(s("1999-12-31"))),
                std(StandardTag::Comment(s("A comment that is longer than twenty-eight"))),
                std(StandardTag::TrackNumber(7)),
                std(StandardTag::Genre(s("rock"))),
            ];

            let buf = serialize(&tags);
            assert_eq!(&buf[..3], b"TAG");
            assert_eq!(buf[127], 17);
            assert_eq!(buf[125], 0);

            let read = read(&buf);
            assert!(read.contains(&StandardTag::TrackTitle(s("Caf\u{e9} \u{20ac}uro"))));
            assert!(read.contains(&StandardTag::Artist(s("Artist"))));
            assert!(read.contains(&StandardTag::Album(s("Album"))));
            assert!(read.contains(&StandardTag::RecordingYear(1999)));
            assert!(read.contains(&StandardTag::TrackNumber(7)));
            assert!(read.contains(&StandardTag::Genre(s("Rock"))));
            // The comment is truncated to 28 characters in ID3v1.1.
            assert!(read.contains(&StandardTag::Comment(s("A comment that is longer tha"))));
        }

        #[test]
        fn verify_long_comment_without_track_uses_30_characters() {
            let buf = serialize(&[std(StandardTag::Comment(s("0123456789012345678901234567890")))]);
            assert!(
                read(&buf).contains(&StandardTag::Comment(s("012345678901234567890123456789")))
            );
            assert_eq!(buf[127], 255);
        }

        #[test]
        fn verify_unrepresentable_characters_and_unknown_genre() {
            let buf = serialize(&[
                std(StandardTag::TrackTitle(s("\u{65e5}\u{672c}"))),
                std(StandardTag::Genre(s("Not A Genre"))),
                std(StandardTag::TrackNumber(300)),
            ]);
            assert_eq!(&buf[3..6], b"??\0");
            assert_eq!(buf[127], 255);
            // A track number that does not fit is not written.
            assert_eq!(buf[126], 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symphonia_core::io::BufReader;

    fn read_tags(comment_tail: [u8; 2]) -> Vec<Tag> {
        let mut buf = [0u8; 128];
        buf[..3].copy_from_slice(b"TAG");
        buf[3..8].copy_from_slice(b"Title");
        buf[97..101].copy_from_slice(b"Note");
        // The last 2 bytes of the comment field.
        buf[125..127].copy_from_slice(&comment_tail);
        buf[127] = 13;

        let mut builder = MetadataBuilder::new(ID3V1_METADATA_INFO);
        read_id3v1(&mut BufReader::new(&buf), None, &mut builder).unwrap();
        builder.build().media.tags
    }

    fn track_numbers(tags: &[Tag]) -> Vec<u64> {
        tags.iter()
            .filter_map(|t| match t.std {
                Some(StandardTag::TrackNumber(n)) => Some(n),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn verify_id3v11_track_number() {
        assert_eq!(track_numbers(&read_tags([0, 7])), [7]);
    }

    /// An ID3v1.1 track number of 0 means no track number.
    #[test]
    fn verify_id3v11_zero_track_number_is_unset() {
        let tags = read_tags([0, 0]);
        assert!(track_numbers(&tags).is_empty());
        assert!(tags.iter().all(|t| t.raw.key != "TRACK"));
        // The comment is still read.
        assert!(
            tags.iter()
                .any(|t| matches!(&t.std, Some(StandardTag::Comment(c)) if c.as_str() == "Note"))
        );
    }

    /// An ID3v1.0 tag has no track number.
    #[test]
    fn verify_id3v10_has_no_track_number() {
        assert!(track_numbers(&read_tags([b'x', 5])).is_empty());
    }

    fn tag_bytes(title: &[u8], genre_idx: u8) -> [u8; 128] {
        let mut buf = [0u8; 128];
        buf[..3].copy_from_slice(b"TAG");
        buf[3..3 + title.len()].copy_from_slice(title);
        buf[127] = genre_idx;
        buf
    }

    fn enhanced_bytes(title: &[u8], artist: &[u8], album: &[u8], genre: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; ID3V1_ENHANCED_LEN];
        buf[..4].copy_from_slice(b"TAG+");
        buf[4..4 + title.len()].copy_from_slice(title);
        buf[64..64 + artist.len()].copy_from_slice(artist);
        buf[124..124 + album.len()].copy_from_slice(album);
        buf[185..185 + genre.len()].copy_from_slice(genre);
        buf
    }

    /// Read the ID3v1 tag at the end of `stream` through the reader, as the probe would.
    fn read_stream(stream: Vec<u8>, seekable: bool) -> Vec<Tag> {
        use std::io::Cursor;
        use symphonia_core::io::{MediaSourceStreamOptions, ReadOnlySource};

        let start = (stream.len() - 128) as u64;
        let source: Box<dyn MediaSource> = if seekable {
            Box::new(Cursor::new(stream))
        }
        else {
            Box::new(ReadOnlySource::new(Cursor::new(stream)))
        };
        let mut mss = MediaSourceStream::new(source, MediaSourceStreamOptions::default());

        if seekable {
            mss.seek(SeekFrom::Start(start)).unwrap();
        }
        else {
            mss.ignore_bytes(start).unwrap();
        }

        let mut reader = Id3v1Reader::try_new(mss, MetadataOptions::default()).unwrap();
        let tags = reader.read_all().unwrap().revision.media.tags;

        if seekable {
            // The stream is positioned immediately after the tag.
            assert_eq!(Box::new(reader).into_inner().pos(), start + 128);
        }

        tags
    }

    fn find_std(tags: &[Tag], f: impl Fn(&StandardTag) -> Option<&Arc<String>>) -> Option<&str> {
        tags.iter().find_map(|t| t.std.as_ref().and_then(&f)).map(|v| v.as_str())
    }

    fn title_of(tags: &[Tag]) -> Option<&str> {
        find_std(tags, |s| match s {
            StandardTag::TrackTitle(v) => Some(v),
            _ => None,
        })
    }

    fn artist(tags: &[Tag]) -> Option<&str> {
        find_std(tags, |s| match s {
            StandardTag::Artist(v) => Some(v),
            _ => None,
        })
    }

    fn genre(tags: &[Tag]) -> Option<&str> {
        find_std(tags, |s| match s {
            StandardTag::Genre(v) => Some(v),
            _ => None,
        })
    }

    /// Trailing spaces and NULs are trimmed from text fields.
    #[test]
    fn verify_trailing_spaces_are_trimmed() {
        let mut title = b"Padded Title".to_vec();
        title.extend_from_slice(&[b' '; 10]);
        let tags = read_stream(tag_bytes(&title, 255).to_vec(), true);
        assert_eq!(title_of(&tags), Some("Padded Title"));

        // A field of only spaces is empty.
        let tags = read_stream(tag_bytes(&[b' '; 30], 255).to_vec(), true);
        assert_eq!(title_of(&tags), None);
    }

    /// C1 bytes in ID3v1 text are decoded as Windows-1252.
    #[test]
    fn verify_windows_1252_text() {
        let tags = read_stream(tag_bytes(b"It\x92s \x93x\x94 \x81", 255).to_vec(), true);
        assert_eq!(title_of(&tags), Some("It\u{2019}s \u{201c}x\u{201d} \u{fffd}"));
    }

    /// The enhanced tag provides the full title, artist, album and the free-text genre.
    #[test]
    fn verify_enhanced_tag() {
        let full_title = "T".repeat(45);
        let mut stream = vec![0xffu8; 300];
        stream.extend(enhanced_bytes(full_title.as_bytes(), b"Long Artist", b"Album", b"Shoegaze"));
        stream.extend(tag_bytes(&full_title.as_bytes()[..30], 13));

        let tags = read_stream(stream, true);

        assert_eq!(title_of(&tags), Some(full_title.as_str()));
        assert_eq!(artist(&tags), Some("Long Artist"));
        assert_eq!(genre(&tags), Some("Shoegaze"));
    }

    /// An empty enhanced tag field falls back to the ID3v1 field.
    #[test]
    fn verify_enhanced_tag_fallback() {
        let mut stream = vec![0xffu8; 300];
        stream.extend(enhanced_bytes(b"", b"", b"", b""));
        stream.extend(tag_bytes(b"Short", 13));

        let tags = read_stream(stream, true);

        assert_eq!(title_of(&tags), Some("Short"));
        assert_eq!(genre(&tags), Some("Pop"));
    }

    /// Without a "TAG+" marker, or without enough data before the tag, the enhanced tag is not
    /// read.
    #[test]
    fn verify_no_enhanced_tag() {
        let mut stream = vec![0xffu8; 300];
        stream.extend(tag_bytes(b"Short", 13));
        assert_eq!(title_of(&read_stream(stream, true)), Some("Short"));

        // Fewer than 227 bytes precede the tag.
        assert_eq!(title_of(&read_stream(tag_bytes(b"Short", 13).to_vec(), true)), Some("Short"));
        let mut stream = vec![0xffu8; 100];
        stream.extend(tag_bytes(b"Short", 13));
        assert_eq!(title_of(&read_stream(stream, true)), Some("Short"));
    }

    /// A non-seekable stream skips the enhanced tag silently.
    #[test]
    fn verify_enhanced_tag_skipped_when_not_seekable() {
        let full_title = "T".repeat(45);
        let mut stream = vec![0xffu8; 300];
        stream.extend(enhanced_bytes(full_title.as_bytes(), b"", b"", b"Shoegaze"));
        stream.extend(tag_bytes(&full_title.as_bytes()[..30], 13));

        let tags = read_stream(stream, false);

        assert_eq!(title_of(&tags), Some(&full_title[..30]));
        assert_eq!(genre(&tags), Some("Pop"));
    }
}
