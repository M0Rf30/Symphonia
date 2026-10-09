// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ID3v1 metadata reader.

use std::sync::Arc;

use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::formats::probe::{
    Anchors, ProbeMetadataData, ProbeableMetadata, Score, Scoreable,
};
use symphonia_core::io::{MediaSourceStream, ReadBytes, ScopedStream};
use symphonia_core::meta::well_known::METADATA_ID_ID3V1;
use symphonia_core::meta::{
    MetadataBuffer, MetadataBuilder, MetadataInfo, MetadataOptions, MetadataReader, StandardTag,
    Tag,
};
use symphonia_core::support_metadata;
use symphonia_core::util::text;

use crate::utils::id3v1::get_genre_name;

fn read_id3v1<B: ReadBytes>(reader: &mut B, builder: &mut MetadataBuilder) -> Result<()> {
    // Read the "TAG" header.
    let marker = reader.read_triple_bytes()?;

    if marker != *b"TAG" {
        return unsupported_error("id3v1: Not an ID3v1 tag");
    }

    let mut buf = [0u8; 125];
    reader.read_buf_exact(&mut buf)?;

    if let Some(title) = decode_iso8859_buf(&buf[0..30]) {
        let tag = Tag::new_from_parts("TITLE", title.clone(), Some(StandardTag::TrackTitle(title)));
        builder.add_tag(tag);
    }

    if let Some(artist) = decode_iso8859_buf(&buf[30..60]) {
        let tag = Tag::new_from_parts("ARTIST", artist.clone(), Some(StandardTag::Artist(artist)));
        builder.add_tag(tag);
    }

    if let Some(album) = decode_iso8859_buf(&buf[60..90]) {
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

    // Convert the genre index to an actual genre name using the GENRES lookup table.
    if let Some(genre) = get_genre_name(buf[124]).map(Arc::new) {
        let tag = Tag::new_from_parts("GENRE", genre.clone(), Some(StandardTag::Genre(genre)));
        builder.add_tag(tag);
    }

    Ok(())
}

fn decode_iso8859_buf(buf: &[u8]) -> Option<Arc<String>> {
    // Decode a null-terminated string. ID3v1 does not specify an encoding, so assume ISO-8859-1
    // such that all characters codes are valid.
    let text =
        text::decode_iso8859_1_lossy(buf).take_while(text::filter::not_null).collect::<String>();

    // Do not return an empty string.
    if !text.is_empty() { Some(Arc::new(text)) } else { None }
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
        read_id3v1(&mut self.reader, &mut builder)?;
        Ok(MetadataBuffer { revision: builder.build(), side_data: Vec::new() })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
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
        read_id3v1(&mut BufReader::new(&buf), &mut builder).unwrap();
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
}
