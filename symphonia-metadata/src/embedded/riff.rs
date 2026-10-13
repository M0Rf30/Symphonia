// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RIFF-based metadata formats reading.

#[cfg(feature = "riff-info")]
mod info {
    //! RIFF INFO chunk metadata format reading.

    use std::collections::HashMap;
    use std::str;

    use lazy_static::lazy_static;

    use symphonia_core::errors::{Result, decode_error};
    use symphonia_core::meta::{MetadataBuilder, RawTag};

    use crate::utils::std_tag::*;
    #[cfg(feature = "write")]
    use symphonia_core::meta::{StandardTag, Tag};

    lazy_static! {
        static ref RIFF_INFO_MAP: RawTagParserMap = {
            let mut m: RawTagParserMap = HashMap::new();
            m.insert("ages", parse_rating);
            m.insert("cmnt", parse_comment);
            m.insert("comm", parse_comment); // TODO: Same as "cmnt"
            m.insert("dtim", parse_recording_time);
            m.insert("genr", parse_genre);
            m.insert("iart", parse_artist);
            m.insert("icmt", parse_comment); // TODO: Same as "cmnt"?
            m.insert("icnt", parse_release_country);
            m.insert("icop", parse_copyright);
            m.insert("icrd", parse_recording_date);
            m.insert("idit", parse_recording_date); // TODO: Actually date of last edit?
            m.insert("ienc", parse_encoded_by);
            m.insert("ieng", parse_engineer);
            m.insert("ifrm", parse_track_total);
            m.insert("ignr", parse_genre);
            m.insert("ilng", parse_language);
            m.insert("imed", parse_media_format);
            m.insert("imus", parse_composer);
            m.insert("inam", parse_track_title);
            m.insert("iprd", parse_album);
            m.insert("ipro", parse_producer);
            m.insert("iprt", parse_track_number_exclusive);
            m.insert("irtd", parse_rating);
            m.insert("isft", parse_encoder);
            m.insert("isgn", parse_genre);
            m.insert("isrf", parse_media_format);
            m.insert("itch", parse_encoded_by);
            m.insert("itoc", parse_cdtoc);
            m.insert("itrk", parse_track_number_exclusive);
            m.insert("iwri", parse_writer);
            m.insert("lang", parse_language);
            m.insert("prt1", parse_part_number_exclusive);
            m.insert("prt2", parse_part_total);
            m.insert("titl", parse_track_title); // TODO: Same as "inam"?
            m.insert("torg", parse_label);
            m.insert("trck", parse_track_number_exclusive);
            m.insert("tver", parse_version);
            m.insert("year", parse_recording_year);
            m
        };
    }

    /// Parse a RIFF INFO chunk into a `Tag` using the chunk's identifier and a slice containing the
    /// chunk's payload.
    pub fn parse_riff_info_chunk(
        chunk_id: [u8; 4],
        buf: &[u8],
        builder: &mut MetadataBuilder,
    ) -> Result<()> {
        // It is invalid for a chunk ID to contain non-ASCII characters, or ASCII control characters
        // per EA-IFF-85. Since chunk IDs are supposed to be well-defined, and each metadata/info
        // chunk has a standardized ID, this is a hard error.
        if chunk_id.iter().any(|c| !c.is_ascii() || c.is_ascii_control()) {
            return decode_error("meta (riff): chunk ID is invalid");
        }

        // Safety: Key is always ASCII.
        let key = str::from_utf8(&chunk_id).expect("chunk_id contains only ASCII bytes");
        // RIFF INFO payloads are NUL-terminated strings, and are NUL- or space-padded to an
        // even length. Those padding bytes are not part of the value.
        let value = String::from_utf8_lossy(buf);
        let value = value.trim_end_matches(['\0', ' ']).to_string();

        builder.add_mapped_tags(RawTag::new(key, value), &RIFF_INFO_MAP);

        Ok(())
    }

    /// Returns `true` if `chunk_id` is an INFO sub-chunk that the reader maps to a standard tag
    /// (compared case-insensitively, as the reader does). A writer replacing an INFO list from a
    /// set of tags drops these sub-chunks from the old list, and keeps all others.
    #[cfg(feature = "write")]
    pub fn is_managed_info_id(chunk_id: [u8; 4]) -> bool {
        let key = chunk_id.to_ascii_lowercase();
        str::from_utf8(&key).is_ok_and(|key| RIFF_INFO_MAP.contains_key(key))
    }

    /// The INFO sub-chunk identifier and text the reader maps back to `std`, or `None` if the INFO
    /// format has no field for it. Each identifier is the first (canonical) key the reader maps to
    /// the same standard tag.
    #[cfg(feature = "write")]
    fn info_field(std: &StandardTag) -> Option<([u8; 4], String)> {
        let text = |id: &[u8; 4], v: &str| Some((*id, v.to_string()));
        let number = |id: &[u8; 4], v: u64| Some((*id, v.to_string()));

        match std {
            StandardTag::TrackTitle(v) => text(b"INAM", v),
            StandardTag::Artist(v) => text(b"IART", v),
            StandardTag::Album(v) => text(b"IPRD", v),
            StandardTag::RecordingDate(v) => text(b"ICRD", v),
            StandardTag::Genre(v) => text(b"IGNR", v),
            StandardTag::Comment(v) => text(b"ICMT", v),
            StandardTag::TrackNumber(v) => number(b"ITRK", *v),
            StandardTag::TrackTotal(v) => number(b"IFRM", *v),
            StandardTag::Copyright(v) => text(b"ICOP", v),
            StandardTag::Encoder(v) => text(b"ISFT", v),
            StandardTag::Engineer(v) => text(b"IENG", v),
            StandardTag::Composer(v) => text(b"IMUS", v),
            StandardTag::Producer(v) => text(b"IPRO", v),
            StandardTag::Language(v) => text(b"ILNG", v),
            StandardTag::MediaFormat(v) => text(b"IMED", v),
            StandardTag::EncodedBy(v) => text(b"IENC", v),
            StandardTag::Writer(v) => text(b"IWRI", v),
            StandardTag::ReleaseCountry(v) => text(b"ICNT", v),
            StandardTag::RecordingTime(v) => text(b"DTIM", v),
            StandardTag::Label(v) => text(b"TORG", v),
            StandardTag::Version(v) => text(b"TVER", v),
            StandardTag::CdToc(v) => text(b"ITOC", v),
            StandardTag::PartNumber(v) => number(b"PRT1", *v),
            StandardTag::PartTotal(v) => number(b"PRT2", *v),
            StandardTag::RecordingYear(v) => number(b"YEAR", u64::from(*v)),
            // The reader scales a 0-100 rating to parts per million. Only ratings that map back
            // exactly are written.
            StandardTag::Rating(ppm) if *ppm <= 1_000_000 && *ppm % 10_000 == 0 => {
                number(b"IRTD", u64::from(*ppm / 10_000))
            }
            _ => None,
        }
    }

    /// Serializes `tags` into a sequence of RIFF INFO sub-chunks, i.e. the payload of a
    /// `LIST` chunk after its `INFO` form type (the caller writes the `LIST` header and form
    /// type).
    ///
    /// Only tags with a standard tag the INFO format has a field for are written; the field is the
    /// first (canonical) key the reader maps to that standard tag (`INAM`, `IART`, `IPRD`, `ICRD`,
    /// `IGNR`, `ICMT`, `ITRK`, `IFRM`, `ICOP`, `ISFT`, `IENG`, `IMUS`, `IPRO`, `ILNG`, `IMED`,
    /// `IENC`, `IWRI`, `ICNT`, `DTIM`, `TORG`, `TVER`, `ITOC`, `PRT1`, `PRT2`, `YEAR`, and `IRTD`
    /// for ratings that are a multiple of 1%). Parsing the result with
    /// [`parse_riff_info_chunk`] yields the same standard tags.
    ///
    /// Several tags for the same standard tag (e.g. two artists) are written as repeated
    /// sub-chunks, in order, which the reader returns as repeated tags. Numeric fields hold a
    /// single number, so only the first such tag is written. Values are written as UTF-8, NUL
    /// terminated, with a pad byte after an odd-length payload (not counted in the sub-chunk size).
    /// Interior NUL bytes are removed, and empty values are skipped.
    #[cfg(feature = "write")]
    pub fn serialize_info(tags: &[Tag]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut seen_numeric: Vec<[u8; 4]> = Vec::new();

        for std in tags.iter().filter_map(|tag| tag.std.as_ref()) {
            let Some((id, value)) = info_field(std)
            else {
                continue;
            };

            let numeric = matches!(
                std,
                StandardTag::TrackNumber(_)
                    | StandardTag::TrackTotal(_)
                    | StandardTag::PartNumber(_)
                    | StandardTag::PartTotal(_)
                    | StandardTag::RecordingYear(_)
                    | StandardTag::Rating(_)
            );

            if numeric {
                if seen_numeric.contains(&id) {
                    continue;
                }
                seen_numeric.push(id);
            }

            let mut payload = value.into_bytes();
            payload.retain(|&b| b != 0);

            if payload.is_empty() {
                continue;
            }

            payload.push(0);

            let Ok(len) = u32::try_from(payload.len())
            else {
                continue;
            };

            out.extend_from_slice(&id);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&payload);

            if len & 1 == 1 {
                out.push(0);
            }
        }

        out
    }

    #[cfg(all(test, feature = "write"))]
    mod tests {
        use std::sync::Arc;

        use symphonia_core::meta::well_known::METADATA_ID_WAVE;
        use symphonia_core::meta::{MetadataInfo, RawTag};

        use super::*;

        fn s(v: &str) -> Arc<String> {
            Arc::new(v.to_string())
        }

        fn tag(std: StandardTag) -> Tag {
            Tag::new_std(RawTag::new("x", "x"), std)
        }

        /// Parses a sequence of INFO sub-chunks back into standard tags.
        fn read_back(mut buf: &[u8]) -> Vec<StandardTag> {
            let mut builder = MetadataBuilder::new(MetadataInfo {
                metadata: METADATA_ID_WAVE,
                short_name: "wave",
                long_name: "Waveform Audio File Format",
            });

            while !buf.is_empty() {
                let id: [u8; 4] = buf[..4].try_into().unwrap();
                let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
                parse_riff_info_chunk(id, &buf[8..8 + len], &mut builder).unwrap();
                // Sub-chunks are word aligned.
                buf = &buf[(8 + len + (len & 1))..];
            }

            builder.build().media.tags.into_iter().filter_map(|t| t.std).collect()
        }

        #[test]
        fn verify_round_trip_of_all_mapped_tags() {
            let tags = vec![
                StandardTag::TrackTitle(s("Title")),
                StandardTag::Artist(s("Artist")),
                StandardTag::Album(s("Album")),
                StandardTag::RecordingDate(s("2024-05-06")),
                StandardTag::Genre(s("Genre")),
                StandardTag::Comment(s("A comment")),
                StandardTag::TrackNumber(3),
                StandardTag::TrackTotal(12),
                StandardTag::Copyright(s("(c) me")),
                StandardTag::Encoder(s("Lavf")),
                StandardTag::Engineer(s("Eng")),
                StandardTag::Composer(s("Comp")),
                StandardTag::Producer(s("Prod")),
                StandardTag::Language(s("eng")),
                StandardTag::MediaFormat(s("CD")),
                StandardTag::EncodedBy(s("Enc")),
                StandardTag::Writer(s("Writer")),
                StandardTag::ReleaseCountry(s("DE")),
                StandardTag::RecordingTime(s("12:00")),
                StandardTag::Label(s("Label")),
                StandardTag::Version(s("v2")),
                StandardTag::CdToc(s("1 2 3")),
                StandardTag::PartNumber(1),
                StandardTag::PartTotal(2),
                StandardTag::RecordingYear(2024),
                StandardTag::Rating(850_000),
            ];

            let input: Vec<Tag> = tags.iter().cloned().map(tag).collect();
            let out = serialize_info(&input);
            assert_eq!(out.len() % 2, 0);
            assert_eq!(read_back(&out), tags);
        }

        #[test]
        fn verify_odd_and_even_values_are_word_aligned() {
            // "ab" + NUL = 3 bytes (odd, padded), "abc" + NUL = 4 bytes (even).
            let out = serialize_info(&[
                tag(StandardTag::TrackTitle(s("ab"))),
                tag(StandardTag::Artist(s("abc"))),
            ]);

            assert_eq!(&out[0..4], b"INAM");
            assert_eq!(&out[4..8], &3u32.to_le_bytes());
            assert_eq!(&out[8..12], b"ab\0\0");
            assert_eq!(&out[12..16], b"IART");
            assert_eq!(&out[16..20], &4u32.to_le_bytes());
            assert_eq!(&out[20..24], b"abc\0");
            assert_eq!(out.len(), 24);
        }

        #[test]
        fn verify_multiple_values_repeat_and_numbers_do_not() {
            let tags = vec![
                StandardTag::Artist(s("A")),
                StandardTag::Artist(s("B")),
                StandardTag::TrackNumber(1),
                StandardTag::TrackNumber(2),
            ];

            let input: Vec<Tag> = tags.iter().cloned().map(tag).collect();
            let back = read_back(&serialize_info(&input));

            assert_eq!(
                back,
                vec![
                    StandardTag::Artist(s("A")),
                    StandardTag::Artist(s("B")),
                    StandardTag::TrackNumber(1)
                ]
            );
        }

        #[test]
        fn verify_unmapped_tags_and_odd_ratings_are_skipped() {
            let out = serialize_info(&[
                tag(StandardTag::Bpm(120)),
                tag(StandardTag::Rating(123_456)),
                tag(StandardTag::Album(s(""))),
                Tag::new(RawTag::new("free", "form")),
            ]);

            assert!(out.is_empty());
        }

        #[test]
        fn verify_managed_ids_are_case_insensitive() {
            assert!(is_managed_info_id(*b"INAM"));
            assert!(is_managed_info_id(*b"inam"));
            assert!(is_managed_info_id(*b"PRT1"));
            assert!(!is_managed_info_id(*b"ISBJ"));
            assert!(!is_managed_info_id(*b"IKEY"));
        }
    }
}

#[cfg(feature = "riff-id3")]
mod id3 {
    //! RIFF ID3 chunk metadata format reading.

    use symphonia_core::errors::Result;
    use symphonia_core::io::ReadBytes;
    use symphonia_core::meta::{MetadataBuilder, MetadataRevision, MetadataSideData};

    /// Read a RIFF ID3 chunk payload.
    pub fn read_riff_id3_chunk<B: ReadBytes>(
        reader: &mut B,
        side_data: &mut Vec<MetadataSideData>,
    ) -> Result<MetadataRevision> {
        let mut builder = MetadataBuilder::new(crate::id3v2::ID3V2_METADATA_INFO);
        crate::id3v2::read_id3v2(reader, &mut builder, side_data)?;
        Ok(builder.build())
    }
}

#[cfg(feature = "riff-id3")]
pub use id3::*;
#[cfg(feature = "riff-info")]
pub use info::*;
