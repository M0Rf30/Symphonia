// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An APEv1 and APEv2 metadata reader.

#[cfg(feature = "write")]
pub mod write;

use core::str;
use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::formats::probe::{
    Anchors, ProbeMetadataData, ProbeableMetadata, Score, Scoreable,
};
use symphonia_core::io::{MediaSourceStream, ReadBytes, ScopedStream, SeekBuffered};
use symphonia_core::meta::well_known::{METADATA_ID_APEV1, METADATA_ID_APEV2};
use symphonia_core::meta::{
    MetadataBuffer, MetadataBuilder, MetadataInfo, MetadataOptions, MetadataReader, RawTag,
    RawValue, StandardTag, StandardVisualKey, Tag,
};
use symphonia_core::support_metadata;

use lazy_static::lazy_static;
use log::{debug, warn};
use symphonia_core::util::text;

use crate::utils::ape::{ApeTagFlags, HEADER_LEN};
use crate::utils::id3v1::decode_windows_1252_byte;
use crate::utils::images::{ImageInfo, try_get_image_info, visual_from_image_data};
use crate::utils::std_tag::*;

lazy_static! {
    static ref APE_TAG_MAP: RawTagParserMap = {
        let mut m: RawTagParserMap = HashMap::new();
        m.insert("accurateripcount"            , parse_accuraterip_count);
        m.insert("accurateripcountalloffsets"  , parse_accuraterip_count_all_offsets);
        m.insert("accurateripcountwithoffset"  , parse_accuraterip_count_with_offset);
        m.insert("accurateripcrc"              , parse_accuraterip_crc);
        m.insert("accurateripdiscid"           , parse_accuraterip_disc_id);
        m.insert("accurateripid"               , parse_accuraterip_id);
        m.insert("accurateripoffset"           , parse_accuraterip_offset);
        m.insert("accurateripresult"           , parse_accuraterip_result);
        m.insert("accurateriptotal"            , parse_accuraterip_total);
        m.insert("acoustid_fingerprint"        , parse_acoustid_fingerprint);
        m.insert("acoustid_id"                 , parse_acoustid_id);
        m.insert("album artist"                , parse_album_artist);
        m.insert("albumartist"                 , parse_album_artist);
        m.insert("album"                       , parse_album);
        m.insert("albumartistsort"             , parse_sort_album_artist);
        m.insert("albumsort"                   , parse_sort_album);
        m.insert("arranger"                    , parse_arranger);
        m.insert("artist"                      , parse_artist);
        m.insert("artistsort"                  , parse_sort_artist);
        m.insert("asin"                        , parse_ident_asin);
        m.insert("bpm"                         , parse_bpm);
        m.insert("catalog"                     , parse_ident_catalog_number);
        m.insert("catalognumber"               , parse_ident_catalog_number);
        m.insert("comment"                     , parse_comment);
        m.insert("compilation"                 , parse_compilation);
        m.insert("composer"                    , parse_composer);
        m.insert("composersort"                , parse_sort_composer);
        m.insert("conductor"                   , parse_conductor);
        m.insert("copyright"                   , parse_copyright);
        // Disc Number or Disc Number/Total Discs
        m.insert("disc"                        , parse_disc_number);
        m.insert("discnumber"                  , parse_disc_number);
        m.insert("disctotal"                   , parse_disc_total);
        m.insert("totaldiscs"                  , parse_disc_total);
        m.insert("djmixer"                     , parse_mix_dj);
        // EAN-13/UPC-A
        m.insert("ean/upc"                     , parse_ident_ean_upn);
        m.insert("encodedby"                   , parse_encoded_by);
        m.insert("encoder settings"            , parse_encoder_settings);
        m.insert("encoder"                     , parse_encoder);
        m.insert("engineer"                    , parse_engineer);
        m.insert("file"                        , parse_original_file);
        m.insert("genre"                       , parse_genre);
        m.insert("grouping"                    , parse_grouping);
        m.insert("isbn"                        , parse_ident_isbn);
        m.insert("isrc"                        , parse_ident_isrc);
        m.insert("label"                       , parse_label);
        m.insert("lc"                          , parse_label_code);
        m.insert("labelcode"                   , parse_label_code);
        m.insert("language"                    , parse_language);
        m.insert("lyricist"                    , parse_lyricist);
        m.insert("lyrics"                      , parse_lyrics);
        m.insert("media"                       , parse_media_format);
        m.insert("mixer"                       , parse_mix_engineer);
        m.insert("mood"                        , parse_mood);
        m.insert("mixartist"                   , parse_remixer);
        m.insert("movement"                    , parse_movement_total);
        m.insert("movementname"                , parse_movement_name);
        m.insert("movementtotal"               , parse_movement_total);
        m.insert("mp3gain_album_minmax"        , parse_mp3gain_album_min_max);
        m.insert("mp3gain_minmax"              , parse_mp3gain_min_max);
        m.insert("mp3gain_undo"                , parse_mp3gain_undo);
        m.insert("musicbrainz_albumartistid"   , parse_musicbrainz_album_artist_id);
        m.insert("musicbrainz_albumid"         , parse_musicbrainz_album_id);
        m.insert("musicbrainz_albumstatus"     , parse_musicbrainz_release_status);
        m.insert("musicbrainz_albumtype"       , parse_musicbrainz_release_type);
        m.insert("musicbrainz_artistid"        , parse_musicbrainz_artist_id);
        m.insert("musicbrainz_discid"          , parse_musicbrainz_disc_id);
        m.insert("musicbrainz_originalalbumid" , parse_musicbrainz_original_album_id);
        m.insert("musicbrainz_originalartistid", parse_musicbrainz_original_artist_id);
        m.insert("musicbrainz_releasegroupid"  , parse_musicbrainz_release_group_id);
        m.insert("musicbrainz_releasetrackid"  , parse_musicbrainz_release_track_id);
        m.insert("musicbrainz_trackid"         , parse_musicbrainz_track_id);
        m.insert("musicbrainz_trmid"           , parse_musicbrainz_trm_id);
        m.insert("musicbrainz_workid"          , parse_musicbrainz_work_id);
        m.insert("original artist"             , parse_original_artist);
        m.insert("performer"                   , parse_performer);
        m.insert("producer"                    , parse_producer);
        m.insert("originalyear"                , parse_original_release_year);
        m.insert("publisher"                   , parse_label);
        m.insert("record date"                 , parse_recording_date);
        m.insert("record location"             , parse_recording_location);
        m.insert("related"                     , parse_url);
        m.insert("remixer"                     , parse_remixer);
        m.insert("replaygain_album_gain"       , parse_replaygain_album_gain);
        m.insert("replaygain_album_peak"       , parse_replaygain_album_peak);
        m.insert("replaygain_track_gain"       , parse_replaygain_track_gain);
        m.insert("replaygain_track_peak"       , parse_replaygain_track_peak);
        m.insert("weblink"                     , parse_url);
        m.insert("subtitle"                    , parse_track_subtitle);
        m.insert("title"                       , parse_track_title);
        m.insert("titlesort"                   , parse_sort_track_title);
        // Track Number or Track Number/Total Tracks
        m.insert("track"                       , parse_track_number);
        m.insert("tracknumber"                 , parse_track_number);
        m.insert("tracktotal"                  , parse_track_total);
        m.insert("totaltracks"                 , parse_track_total);
        m.insert("writer"                      , parse_writer);
        m.insert("year"                        , parse_release_date);

        // No mappings for: Index, Introplay, Dummy. Also unmapped, since no standard tag has the
        // same meaning (APE key list, Frank Klemm): Debut Album (the artist's debut album),
        // Publicationright (publication right holder, not the copyright holder), Abstract and
        // Bibliography (links). Unmapped items are kept as raw tags.
        m
    };
}

/// The item keys of cover art items and the usage of the visual of each. The first key of a usage
/// is the key that is written for it.
const COVER_ART_KEYS: &[(&str, StandardVisualKey)] = &[
    ("Cover Art (Other)", StandardVisualKey::Other),
    ("Cover Art (PNG Icon)", StandardVisualKey::FileIcon),
    ("Cover Art (Icon)", StandardVisualKey::OtherIcon),
    ("Cover Art (Front)", StandardVisualKey::FrontCover),
    ("Cover Art (Back)", StandardVisualKey::BackCover),
    ("Cover Art (Leaflet)", StandardVisualKey::Leaflet),
    ("Cover Art (Media)", StandardVisualKey::Media),
    ("Cover Art (Lead Artist)", StandardVisualKey::LeadArtistPerformerSoloist),
    ("Cover Art (Artist)", StandardVisualKey::ArtistPerformer),
    ("Cover Art (Conductor)", StandardVisualKey::Conductor),
    ("Cover Art (Band)", StandardVisualKey::BandOrchestra),
    ("Cover Art (Composer)", StandardVisualKey::Composer),
    ("Cover Art (Lyricist)", StandardVisualKey::Lyricist),
    ("Cover Art (Recording Location)", StandardVisualKey::RecordingLocation),
    ("Cover Art (During Recording)", StandardVisualKey::RecordingSession),
    ("Cover Art (During Performance)", StandardVisualKey::Performance),
    ("Cover Art (Video Capture)", StandardVisualKey::ScreenCapture),
    ("Cover Art (Fish)", StandardVisualKey::Other),
    ("Cover Art (Illustration)", StandardVisualKey::Illustration),
    ("Cover Art (Band Logotype)", StandardVisualKey::BandArtistLogo),
    ("Cover Art (Publisher Logotype)", StandardVisualKey::PublisherStudioLogo),
];

lazy_static! {
    static ref APE_VISUAL_TAG_MAP: HashMap<String, StandardVisualKey> =
        COVER_ART_KEYS.iter().map(|&(key, usage)| (key.to_ascii_lowercase(), usage)).collect();
}

const APEV1_METADATA_INFO: MetadataInfo =
    MetadataInfo { metadata: METADATA_ID_APEV1, short_name: "apev1", long_name: "APEv1" };
const APEV2_METADATA_INFO: MetadataInfo =
    MetadataInfo { metadata: METADATA_ID_APEV2, short_name: "apev2", long_name: "APEv2" };

/// The APE tag version.
#[derive(PartialEq, Eq)]
enum ApeVersion {
    /// Version 1, maps to 1000.
    V1,
    /// Version 2, maps to 2000.
    V2,
}

struct ApeHeader {
    version: ApeVersion,
    num_items: u32,
    size: u64,
    is_header: bool,
    has_header: bool,
    has_footer: bool,
}

impl ApeHeader {
    /// The length of the items of the tag in bytes. `size` is the length of the whole tag: the
    /// items, the footer, and the header if the tag has one.
    fn items_len(&self) -> u64 {
        let header_len = if self.has_header { HEADER_LEN } else { 0 };
        self.size.saturating_sub(header_len).saturating_sub(HEADER_LEN)
    }

    /// Read and verify the APE tag preamble and version.
    fn read_identity<B: ReadBytes>(reader: &mut B) -> Result<ApeVersion> {
        let mut preamble = [0; 8];
        reader.read_buf_exact(&mut preamble)?;

        if preamble != *b"APETAGEX" {
            return decode_error("ape: invalid preamble");
        }

        // Read the version. 1000 for APEv1, 2000 for APEv2, and so on...
        let version = match reader.read_u32()? {
            1000 => ApeVersion::V1,
            2000 => ApeVersion::V2,
            _ => return unsupported_error("ape: unsupported version"),
        };

        Ok(version)
    }

    /// Read an APE tag header.
    fn read<B: ReadBytes>(reader: &mut B) -> Result<ApeHeader> {
        let version = ApeHeader::read_identity(reader)?;

        // The size of the tag excluding any header.
        let size = u64::from(reader.read_u32()?);
        let num_items = reader.read_u32()?;
        let flags = reader.read_u32()?;
        let _reserved = reader.read_u64()?;

        // Interpret the flags and size based on version.
        let (size, has_footer, has_header, is_header) = match version {
            ApeVersion::V1 => {
                // Flags should be ignored reading an APEv1 tag. However, an APEv1 tag always has a
                // footer.
                (size, true, false, false)
            }
            ApeVersion::V2 => {
                let flags = ApeTagFlags::from_bits(flags);

                // The header size is not included in the size written to the tag.
                let real_size = flags.tag_len(size);

                (real_size, flags.has_footer, flags.has_header, flags.is_header)
            }
        };

        Ok(ApeHeader { version, num_items, size, is_header, has_header, has_footer })
    }
}

/// The value of an APE tag item.
enum ApeItemValue {
    String(String),
    Binary(Box<[u8]>),
    Locator(String),
}

/// An APE tag item.
struct ApeItem {
    key: String,
    value: ApeItemValue,
}

impl ApeItem {
    /// Try to read and return an APE tag item. `None` is returned if the item was skipped.
    fn read<B: ReadBytes>(
        reader: &mut B,
        header: &ApeHeader,
        items_end: u64,
    ) -> Result<Option<ApeItem>> {
        // The length of the value in bytes.
        let len = reader.read_u32()? as usize;

        // Read flags.
        let flags = match header.version {
            ApeVersion::V1 => {
                // Ignore item flags for APEv1. The value type is always text.
                reader.read_u32()?;
                0
            }
            ApeVersion::V2 => reader.read_u32()?,
        };

        // Read the null-terminated key.
        let key = read_key(reader)?;

        // The value must be within the items of the tag.
        if len as u64 > items_end.saturating_sub(reader.pos()) {
            return decode_error("ape: item value exceeds the tag");
        }

        // Read the value.
        let value = match (flags >> 1) & 0x3 {
            // UTF-8
            0 => ApeItemValue::String(read_utf8_value(reader, len)?),
            // Binary
            1 => ApeItemValue::Binary(reader.read_boxed_slice_exact(len)?),
            // Locator
            2 => ApeItemValue::Locator(read_utf8_value(reader, len)?),
            // Reserved. The length of the value is known, so skip the item and keep reading the tag.
            3 => {
                reader.ignore_bytes(len as u64)?;
                warn!("ape: skipping item '{key}' with a reserved value type");
                return Ok(None);
            }
            _ => unreachable!(),
        };

        Ok(Some(ApeItem { key, value }))
    }
}

/// APEv1 and APEv2 tag reader.
pub struct ApeReader<'s> {
    reader: MediaSourceStream<'s>,
    version: ApeVersion,
}

impl<'s> ApeReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, _opts: MetadataOptions) -> Result<Self> {
        // Read and verify the APE tag preamble and version.
        let version = ApeHeader::read_identity(&mut mss)?;
        mss.seek_buffered_rel(-12);

        Ok(Self { reader: mss, version })
    }
}

impl Scoreable for ApeReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableMetadata<'_> for ApeReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: MetadataOptions,
    ) -> Result<Box<dyn MetadataReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(ApeReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeMetadataData] {
        &[
            // APEv1
            support_metadata!(
                APEV1_METADATA_INFO,
                &[],
                &[],
                &[b"APETAGEX\xe8\x03\x00\x00"],
                // APEv1 tags are only appended to the end of the stream.
                Anchors::Exclusive(&[
                    32,  // APE tag at end of stream.
                    160  // APE tag before ID3v1 tag.
                ])
            ),
            // APEv2
            support_metadata!(
                APEV2_METADATA_INFO,
                &[],
                &[],
                &[b"APETAGEX\xd0\x07\x00\x00"],
                // APEv2 tags can be appended to the end of the stream, or be at the start.
                Anchors::Supplemental(&[
                    32,  // APE tag at end of stream.
                    160  // APE tag before ID3v1 tag.
                ])
            ),
        ]
    }
}

impl MetadataReader for ApeReader<'_> {
    fn metadata_info(&self) -> &MetadataInfo {
        match self.version {
            ApeVersion::V1 => &APEV1_METADATA_INFO,
            ApeVersion::V2 => &APEV2_METADATA_INFO,
        }
    }

    fn read_all(&mut self) -> Result<MetadataBuffer> {
        let mut builder = MetadataBuilder::new(*self.metadata_info());

        // Read the tag header. This may actually be the header OR the footer.
        let header = ApeHeader::read(&mut self.reader)?;

        // If the header was actually a footer. Seek to the start of the APE tag.
        if !header.is_header {
            // The position after the footer is the end of the tag.
            let tag_end = self.reader.pos();

            // The current position is the first byte after the APE footer. After the seek, the
            // reader will be at the header (if the tag contains one), or the first item.
            self.reader.seek(SeekFrom::Current(-(header.size as i64)))?;

            // If the APE tag contains a header, read it and do some verification checks. All header
            // and footer fields should match other than the `is_header` flag.
            if header.has_header {
                match ApeHeader::read(&mut self.reader) {
                    Ok(real_header) => {
                        if header.has_footer != real_header.has_footer
                            || header.has_header != real_header.has_header
                            || header.is_header == real_header.is_header
                            || header.num_items != real_header.num_items
                            || header.size != real_header.size
                            || header.version != real_header.version
                        {
                            return decode_error("ape: header and footer mismatch");
                        }
                    }
                    Err(_) => {
                        // Some writers set the header flag without writing a header. The size
                        // does not include the header, so the items are still found by seeking
                        // back from the end of the tag.
                        warn!("ape: the footer indicates a header, but there is none");

                        let items_start = tag_end.saturating_sub(header.size - HEADER_LEN);
                        self.reader.seek(SeekFrom::Start(items_start))?;
                    }
                }
            }
        }

        // Read APE tag items. The items are bounded by the size of the tag. The item count is not
        // trusted: a count that is too large would otherwise read the footer, or the audio data
        // that follows the tag, as items.
        let items_end = self.reader.pos().saturating_add(header.items_len());

        for _ in 0..header.num_items {
            // The smallest item is the value length, flags, a 2 character key, and the key's
            // null-terminator.
            if items_end.saturating_sub(self.reader.pos()) < MIN_ITEM_LEN {
                warn!("ape: the tag has fewer items than indicated");
                break;
            }

            let item = match ApeItem::read(&mut self.reader, &header, items_end) {
                Ok(Some(item)) => item,
                Ok(None) => continue,
                Err(err) => {
                    // The structure of the remaining items is lost, but the items that were read
                    // are good.
                    warn!("ape: stopped reading items: {err}");
                    break;
                }
            };

            let key_lower = item.key.to_ascii_lowercase();

            // If the APE tag key can be mapped to a standard visual key, and the value is binary
            // data, then consider the tag to be a visual.
            if let Some(std_key) = APE_VISUAL_TAG_MAP.get(key_lower.as_str()).copied() {
                if let ApeItemValue::Binary(data) = item.value {
                    let mut tags = vec![];

                    // Try to parse the image data to obtain information about the image. This may
                    // alter the image buffer if extra information was attached to it.
                    let (data, image_info) = try_parse_image_data(data, &mut tags);

                    builder.add_visual(visual_from_image_data(
                        data,
                        image_info,
                        None,
                        None,
                        Some(std_key),
                        tags,
                    ));

                    continue;
                }
            }

            // Map APE tag item values to raw values.
            let value = match item.value {
                ApeItemValue::String(value) | ApeItemValue::Locator(value) => {
                    // If the value contains a null-terminator, then the value is actually a list.
                    if value.contains('\0') {
                        let items: Vec<String> =
                            value.split_terminator('\0').map(|s| s.to_string()).collect();

                        // The values of a known item are separate tags so that each of them gets
                        // a standard tag.
                        if APE_TAG_MAP.contains_key(key_lower.as_str()) {
                            for value in items {
                                builder.add_mapped_tags(
                                    RawTag::new(item.key.clone(), value),
                                    &APE_TAG_MAP,
                                );
                            }
                            continue;
                        }

                        RawValue::StringList(Arc::new(items))
                    }
                    else {
                        RawValue::from(value)
                    }
                }
                ApeItemValue::Binary(value) => RawValue::from(value),
            };

            builder.add_mapped_tags(RawTag::new(item.key, value), &APE_TAG_MAP);
        }

        // Skip over any unread items so the reader is positioned at the footer.
        let pos = self.reader.pos();

        if pos < items_end {
            self.reader.ignore_bytes(items_end - pos)?;
        }

        // Read the footer. All the items have been read at this point, so a missing or
        // inconsistent footer must not discard them.
        //
        // If the initial header was the actual header, then this checks the entire APE tag was
        // read, and the footer matches the header. If the initial header was actually the footer,
        // or there was no header, then this only checks the entire tag APE tag was read. However,
        // if there was a header, then the header and footer was checked to match earlier above.
        match ApeHeader::read(&mut self.reader) {
            Ok(footer)
                if header.has_footer != footer.has_footer
                    || header.has_header != footer.has_header
                    || header.num_items != footer.num_items
                    || header.size != footer.size
                    || header.version != footer.version =>
            {
                warn!("ape: header and footer mismatch");
            }
            Ok(_) => (),
            Err(err) => warn!("ape: failed to read the footer: {err}"),
        }

        Ok(MetadataBuffer { revision: builder.build(), side_data: Vec::new() })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

/// The minimum length of an item in bytes.
const MIN_ITEM_LEN: u64 = 4 + 4 + 2 + 1;

/// The maximum length of an item key in bytes.
const MAX_KEY_LEN: usize = 255;

fn read_key<B: ReadBytes>(reader: &mut B) -> Result<String> {
    // A key is recommended to be between 2-16 characters.
    let mut key = String::with_capacity(16);
    let mut len = 0;

    loop {
        let c = char::from(reader.read_u8()?);

        // Break at the null-terminator. Do not add it to the string buffer.
        if text::filter::null(&c) {
            break;
        }

        len += 1;

        if len > MAX_KEY_LEN {
            return decode_error("ape: item key is too long");
        }

        // A key may only contain ASCII characters from 0x20 ' ' up to 0x7e '~'.
        if text::filter::ascii_text(&c) {
            key.push(c);
        }
    }

    Ok(key)
}

/// Read a text value of `len` bytes. Items are specified to be UTF-8, but APEv1 does not specify
/// an encoding and many taggers write the system's ANSI code page. A value that is not valid UTF-8
/// is therefore decoded as Windows-1252 rather than discarding the whole tag.
fn read_utf8_value<B: ReadBytes>(reader: &mut B, len: usize) -> Result<String> {
    let buf = reader.read_boxed_slice_exact(len)?.into_vec();

    Ok(String::from_utf8(buf).unwrap_or_else(|err| {
        debug!("ape: item value is not valid utf-8, decoding as windows-1252");
        err.as_bytes().iter().copied().map(decode_windows_1252_byte).collect()
    }))
}

fn try_parse_image_data(buf: Box<[u8]>, tags: &mut Vec<Tag>) -> (Box<[u8]>, Option<ImageInfo>) {
    // It appears that the buffer stored by some binary tag items start with a null-terminated
    // filename of unspecified encoding (though UTF-8 seems likely). This is not documented
    // anywhere. Try to get this filename and strip it from the binary data.

    // Try to detect an image at the start of the data buffer.
    if let Some(info) = try_get_image_info(&buf) {
        // Image detected, return the original buffer back with the image information.
        return (buf, Some(info));
    }

    // Image information could not be detected. The data buffer may start with a null-terminated
    // filename. Try to find a null-terminator.
    if let Some(pos) = buf.iter().position(|&d| d == b'\0') {
        // Split at the null-terminator.
        let (left, right) = buf.split_at(pos);
        // Drop the null-terminator.
        let right = right.split_first().expect("right starts with null byte at pos").1;

        // Try to detect an image after the null-terminator.
        if let Some(info) = try_get_image_info(right) {
            // Try to interpret the bytes preceding the null-terminator as a UTF-8 encoded filename
            // and add it to the visual's tags if successful.
            if let Ok(name) = str::from_utf8(left) {
                if !name.is_empty() {
                    let name = Arc::new(name.to_string());

                    let tag = Tag::new_from_parts(
                        "FILE",
                        name.clone(),
                        Some(StandardTag::OriginalFile(name)),
                    );

                    tags.push(tag);
                }
            }

            // Image detected, return the cropped buffer with the image information.
            return (right.into(), Some(info));
        }
    }

    // An image could not be detected. The image format may be unsupported, or the buffer contains
    // something else. Return the original buffer.
    (buf, None)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Seek};

    use symphonia_core::errors::Error;
    use symphonia_core::io::{BufReader, FiniteStream, MediaSourceStream};
    use symphonia_core::meta::{MetadataOptions, MetadataReader, StandardTag, Tag};

    use super::{ApeReader, MAX_KEY_LEN, read_key};

    fn item(key: &str, flags: u32, value: &[u8]) -> Vec<u8> {
        let mut b = (value.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(key.as_bytes());
        b.push(0);
        b.extend_from_slice(value);
        b
    }

    /// An APEv2 tag with a header and a footer.
    fn tag(items: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = items.concat();
        let size = (body.len() + 32) as u32;
        let block = |flags: u32| {
            let mut b = b"APETAGEX".to_vec();
            b.extend_from_slice(&2000u32.to_le_bytes());
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&(items.len() as u32).to_le_bytes());
            b.extend_from_slice(&flags.to_le_bytes());
            b.extend_from_slice(&[0; 8]);
            b
        };

        [block(0xa000_0000), body, block(0x8000_0000)].concat()
    }

    fn read_tags(tag: Vec<u8>) -> Vec<Tag> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(tag)), Default::default());
        ApeReader::try_new(mss, MetadataOptions::default())
            .unwrap()
            .read_all()
            .unwrap()
            .revision
            .media
            .tags
    }

    /// A footer-only APEv2 tag (the footer is the only block) with the given item count and flags.
    fn footer_only_tag(items: &[Vec<u8>], count: u32, flags: u32) -> Vec<u8> {
        let body: Vec<u8> = items.concat();
        let mut b = body.clone();
        b.extend_from_slice(b"APETAGEX");
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend_from_slice(&((body.len() + 32) as u32).to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&[0; 8]);

        // Place the tag after some stream data.
        [vec![0x55; 64], b].concat()
    }

    fn read_tags_at_end(stream: Vec<u8>) -> Vec<Tag> {
        let end = stream.len() as u64;
        let mut mss = MediaSourceStream::new(Box::new(Cursor::new(stream)), Default::default());
        mss.seek(std::io::SeekFrom::Start(end - 32)).unwrap();
        ApeReader::try_new(mss, MetadataOptions::default())
            .unwrap()
            .read_all()
            .unwrap()
            .revision
            .media
            .tags
    }

    /// An item count that is larger than the number of items in the tag does not discard the
    /// items, nor read the stream data before the tag as items.
    #[test]
    fn verify_item_count_larger_than_items_in_tag() {
        let items = [item("Title", 0, b"x")];
        let tags = read_tags_at_end(footer_only_tag(&items, u32::MAX, 0));

        assert_eq!(tags.len(), 1);
    }

    /// A footer that has the header flag but is not preceded by a header still yields the items.
    #[test]
    fn verify_header_flag_without_header() {
        let items = [item("Title", 0, b"x")];
        let tags = read_tags_at_end(footer_only_tag(&items, 1, 0x8000_0000));

        assert_eq!(tags.len(), 1);
    }

    /// A value that is not valid UTF-8 is decoded as Windows-1252, and a reserved item type is
    /// skipped, without discarding the other items of the tag.
    #[test]
    fn verify_non_utf8_value_and_reserved_item_type_do_not_discard_tag() {
        let tags = read_tags(tag(&[
            item("Title", 0, b"Caf\xe9"),
            item("Junk", 6, b"abc"),
            item("Albumartist", 0, b"Artist"),
        ]));

        assert!(tags.iter().any(|t| {
            matches!(&t.std, Some(StandardTag::TrackTitle(v)) if v.as_str() == "Caf\u{e9}")
        }));
        assert!(tags.iter().any(|t| {
            matches!(&t.std, Some(StandardTag::AlbumArtist(v)) if v.as_str() == "Artist")
        }));
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn verify_read_key_max_length() {
        let mut buf = vec![b'a'; MAX_KEY_LEN];
        buf.push(0);

        let key = read_key(&mut BufReader::new(&buf)).unwrap();
        assert_eq!(key.len(), MAX_KEY_LEN);
    }

    #[test]
    fn verify_read_key_too_long_is_an_error() {
        let mut buf = vec![b'a'; MAX_KEY_LEN + 1];
        buf.push(0);

        assert!(matches!(read_key(&mut BufReader::new(&buf)), Err(Error::DecodeError(_))));

        // A key without a null-terminator must stop at the limit rather than read to the end.
        let buf = vec![b'a'; 1024];
        let mut reader = BufReader::new(&buf);

        assert!(matches!(read_key(&mut reader), Err(Error::DecodeError(_))));
        assert_eq!(reader.bytes_available(), (1024 - MAX_KEY_LEN - 1) as u64);
    }

    #[test]
    fn verify_read_key_counts_dropped_characters() {
        let mut buf = vec![0xff; MAX_KEY_LEN + 1];
        buf.push(0);

        assert!(matches!(read_key(&mut BufReader::new(&buf)), Err(Error::DecodeError(_))));
    }
}
