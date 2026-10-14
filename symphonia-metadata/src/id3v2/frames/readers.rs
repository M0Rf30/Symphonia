// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Frame body readers.

use std::char;
use std::collections::HashMap;
use std::io;
use std::str;
use std::sync::Arc;

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BufReader, FiniteStream, ReadBytes};
use symphonia_core::meta::RawTag;
use symphonia_core::meta::RawTagSubField;
use symphonia_core::meta::{Chapter, RawValue, StandardTag, Tag};
use symphonia_core::units::Time;
use symphonia_core::util::text;

use lazy_static::lazy_static;
use log::debug;
use smallvec::SmallVec;

use crate::id3v2::frames::{FrameResult, Id3v2Chapter, Id3v2TableOfContents};
use crate::id3v2::sub_fields::*;
use crate::utils::id3v1::decode_windows_1252_byte;
use crate::utils::id3v2::get_visual_key_from_picture_type;
use crate::utils::std_tag::*;

use crate::utils::images::{try_get_image_info, visual_from_image_data};

/// The owner identifier of a `UFID` frame containing a MusicBrainz recording ID.
const UFID_OWNER_MUSICBRAINZ: &str = "http://musicbrainz.org";

/// Function pointer to an ID3v2 frame reader.
pub type FrameReader = fn(BufReader<'_>, &FrameInfo<'_>) -> Result<FrameResult>;

/// Map of 4 character ID3v2 frame IDs to a frame reader and optional raw tag parser pair.
pub type FrameReaderMap = HashMap<&'static [u8; 4], (FrameReader, Option<RawTagParser>)>;

/// Useful information about a frame for a frame reader.
pub struct FrameInfo<'a> {
    /// The original ID of the frame as written in the frame.
    id: &'a str,
    /// The major version of the ID3v2 tag containing the frame.
    major_version: u8,
    /// An optional raw tag parser to be applied if the frame is generic.
    raw_tag_parser: Option<RawTagParser>,
}

impl<'a> FrameInfo<'a> {
    /// Create new frame information from a pre-validated frame ID, tag version and optional
    /// raw tag parser.
    ///
    /// Panics if the frame ID is invalid.
    pub fn new(id: &'a [u8], major_version: u8, raw_tag_parser: Option<RawTagParser>) -> Self {
        FrameInfo {
            id: std::str::from_utf8(id).expect("validated frame id bytes"),
            major_version,
            raw_tag_parser,
        }
    }
}

/// Enumeration of valid encodings for text fields in ID3v2 tags
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Encoding {
    /// ISO-8859-1 (aka Latin-1) characters in the range 0x20-0xFF.
    Iso8859_1,
    /// UTF-16 (or UCS-2) with a byte-order-mark (BOM). If the BOM is missing, big-endian encoding
    /// is assumed.
    Utf16Bom,
    /// UTF-16 big-endian without a byte-order-mark (BOM).
    Utf16Be,
    /// UTF-8.
    Utf8,
}

impl Encoding {
    fn parse(encoding: u8) -> Option<Encoding> {
        match encoding {
            // ISO-8859-1 terminated with 0x00.
            0 => Some(Encoding::Iso8859_1),
            // UTF-16 with byte order marker (BOM), terminated with 0x00 0x00.
            1 => Some(Encoding::Utf16Bom),
            // UTF-16BE without byte order marker (BOM), terminated with 0x00 0x00.
            2 => Some(Encoding::Utf16Be),
            // UTF-8 terminated with 0x00.
            3 => Some(Encoding::Utf8),
            // Invalid encoding.
            _ => None,
        }
    }
}

/// Decodes a slice of bytes containing encoded text into a `String`.
///
/// The ID3v2 specification forbids all control characters other than line-feed on the
/// ISO/IEC 8859-1 text encoding, however, does not state if the same limitation applies to the
/// Unicode encodings. Therefore, this restriction is not applied to other encodings.
fn decode_string_buf(buf: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Iso8859_1 => {
            // Decode as an ID3v2-specific variant of ISO/IEC 8859-1 that allows the line-feed
            // control character.
            decode_id3v2_iso8859_1(buf).collect()
        }
        Encoding::Utf8 => {
            // Decode as UTF-8.
            String::from_utf8_lossy(buf).into_owned()
        }
        Encoding::Utf16Bom if is_bomless_utf16le(buf) => {
            // Decode as UTF-16LE, the byte order of the writers that omit the BOM.
            text::decode_utf16le_lossy(buf).collect()
        }
        Encoding::Utf16Bom | Encoding::Utf16Be => {
            // Decode as UTF-16. If a byte-order-mark is present, it will be respected. Otherwise,
            // big-endian is assumed.
            text::decode_utf16be_lossy(buf).collect()
        }
    }
}

/// Returns `true` if `buf`, the text of a frame with the "UTF-16 with BOM" encoding, has no BOM
/// and is most likely little-endian.
///
/// The specification requires a BOM for this encoding, but some writers (that use the native byte
/// order of Windows) omit it. Without a BOM, the byte order is guessed from the position of the
/// zero bytes: the high byte of the Latin characters of a string is 0x00, so it is at the odd
/// offsets of little-endian text, and the even offsets of big-endian text. Big-endian is assumed
/// if the text is not conclusive.
fn is_bomless_utf16le(buf: &[u8]) -> bool {
    if matches!(buf.first_chunk::<2>(), Some([0xfe, 0xff] | [0xff, 0xfe])) {
        return false;
    }

    let (even, odd) = buf.chunks_exact(2).fold((0usize, 0usize), |(even, odd), pair| {
        (even + usize::from(pair[0] == 0), odd + usize::from(pair[1] == 0))
    });

    odd > even
}

fn decode_id3v2_iso8859_1(buf: &[u8]) -> impl Iterator<Item = char> + '_ {
    buf.iter().map(|&c| {
        match c {
            // C0 control codes excluding line-feed.
            0x00..=0x09 | 0x0b..=0x1f => char::REPLACEMENT_CHARACTER,
            // Bytes 0x80 to 0x9f are C1 control codes in ISO-8859-1, but in practice are almost
            // always Windows-1252 text. All other bytes map to the same Unicode code point.
            _ => decode_windows_1252_byte(c),
        }
    })
}

// Primitive value readers (keep sorted in alphabetical order)
//------------------------------------------------------------

/// Read and validate a date.
fn read_date(reader: &mut BufReader<'_>) -> Result<String> {
    // Read an 8 character unterminated date string in the format "YYYYMMDD".
    let mut date = [0; 8];
    reader.read_buf_exact(&mut date)?;

    // All characters must be digits.
    if date.iter().any(|c| !c.is_ascii_digit()) {
        return decode_error("date format is invalid");
    }

    // Safety: The data array only contains ASCII digits.
    Ok(str::from_utf8(&date).expect("date contains only ASCII digits").to_string())
}

/// Read and validate an encoding indicator.
fn read_encoding(reader: &mut BufReader<'_>) -> Result<Encoding> {
    match Encoding::parse(reader.read_byte()?) {
        Some(encoding) => Ok(encoding),
        _ => decode_error("invalid text encoding"),
    }
}

/// Read and validate a language code.
///
/// Language codes must conform to the ISO-639-2 standard. All codes should be composed of 3
/// alphabetic characters. ID3v2 further specifies a language code of "XXX" indicates an unknown or
/// unspecified language.
fn read_lang_code(reader: &mut BufReader<'_>) -> Result<Option<String>> {
    let code = reader.read_triple_bytes()?;

    if code.iter().any(|&c| !c.is_ascii_alphabetic()) {
        return decode_error("invalid language code");
    }

    let code = if code.eq_ignore_ascii_case(b"XXX") {
        // Unknown language code.
        None
    }
    else {
        // Convert to lowercase string.
        Some(
            std::str::from_utf8(&code)
                .expect("lang code contains only ASCII alphabetic chars")
                .to_ascii_lowercase(),
        )
    };

    Ok(code)
}

/// Read and validate the remainder of the buffer as a variably sized play counter.
fn read_play_counter(reader: &mut BufReader<'_>) -> Result<Option<u64>> {
    let len = reader.bytes_available() as usize;

    // A length of 0 indicates no play counter.
    if len == 0 {
        return Ok(None);
    }

    // A valid play counter must be a minimum of 4 bytes long.
    if len < 4 {
        return decode_error("play counter must be a minimum of 32 bits");
    }

    // However it may be extended by an arbitrary amount of bytes (or so it would seem).
    // Practically, a 4-byte (32-bit) count is way more than enough, but we'll support up-to an
    // 8-byte (64bit) count.
    if len > 8 {
        return unsupported_error("play counter greater-than 64 bits are not supported");
    }

    // The play counter is stored as an N-byte big-endian integer. Read N bytes into an 8-byte
    // buffer, making sure the missing bytes are zeroed, and then reinterpret as a 64-bit integer.
    let mut buf = [0u8; 8];
    reader.read_buf_exact(&mut buf[8 - len..])?;

    Ok(Some(u64::from_be_bytes(buf)))
}

/// Read a null-terminated string of the specified encoding from the stream. If the stream ends
/// before the null-terminator is reached, all the bytes up-to that point are interpreted as the
/// string.
fn read_string(reader: &mut BufReader<'_>, encoding: Encoding) -> io::Result<String> {
    let max_len = reader.bytes_available() as usize;

    let buf = match encoding {
        Encoding::Iso8859_1 | Encoding::Utf8 => {
            // Byte aligned encodings. The null-terminator is 1 byte.
            let buf = reader.scan_bytes_aligned_ref(&[0x00], 1, max_len)?;
            // Trim the trailing null-terminator, if present.
            match buf.last() {
                Some(b'\0') => &buf[..buf.len() - 1],
                _ => buf,
            }
        }
        Encoding::Utf16Bom | Encoding::Utf16Be => {
            // Two-byte aligned encodings. The null-terminator is 2 bytes.
            let buf = reader.scan_bytes_aligned_ref(&[0x00, 0x00], 2, max_len)?;
            // Trim the trailing null-terminator, if present.
            match buf.last_chunk::<2>() {
                Some(b"\0\0") => &buf[..buf.len() - 2],
                _ => buf,
            }
        }
    };

    Ok(decode_string_buf(buf, encoding))
}

/// Same behaviour as `read_string`, but ignores empty strings.
fn read_string_ignore_empty(
    reader: &mut BufReader<'_>,
    encoding: Encoding,
) -> io::Result<Option<String>> {
    Ok(Some(read_string(reader, encoding)?).filter(|text| !text.is_empty()))
}

/// Reads a list of strings where each string is null-terminated.
fn read_string_list(reader: &mut BufReader<'_>, encoding: Encoding) -> Result<RawValue> {
    // Optimize for the single string case.
    let mut items: SmallVec<[String; 1]> = Default::default();

    // Read the first string. If the reader is empty, this will push an empty string.
    items.push(read_string(reader, encoding)?);

    // Read additional strings.
    while reader.bytes_available() > 0 {
        items.push(read_string(reader, encoding)?);
    }

    let value = match items.len() {
        0 => unreachable!(),
        1 => RawValue::from(items.remove(0)),
        _ => RawValue::from(items.into_vec()),
    };

    Ok(value)
}

// Frame Readers (keep sorted in alphabetical order)
//--------------------------------------------------

/// Reads an `AENC` (audio encryption) frame.
pub fn read_aenc_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The owner identifier string.
    let owner = read_string(&mut reader, Encoding::Iso8859_1)?;
    // Unencypted, "preview", audio start position in frames.
    let preview_start = reader.read_be_u16()?;
    // Unencrypted, "preview", audio length in frames.
    let preview_length = reader.read_be_u16()?;
    // The remainder of the frame is the binary encryption information.
    let encrypt_info = reader.read_buf_bytes_available_ref();

    let sub_fields = vec![
        RawTagSubField::new(AENC_OWNER, owner),
        RawTagSubField::new(AENC_PREVIEW_START, preview_start),
        RawTagSubField::new(AENC_PREVIEW_LENGTH, preview_length),
    ];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, encrypt_info, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads an `APIC` (attached picture) frame.
pub fn read_apic_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the text description.
    let encoding = read_encoding(&mut reader)?;

    // Image format/media type
    let media_type = if frame.id == "PIC" {
        // Legacy PIC frames use a 3 character identifier. Only JPG and PNG are well-defined.
        match &reader.read_triple_bytes()? {
            b"JPG" => Some("image/jpeg"),
            b"PNG" => Some("image/png"),
            b"BMP" => Some("image/bmp"),
            b"GIF" => Some("image/gif"),
            _ => None,
        }
        .map(|s| s.to_string())
    }
    else {
        // APIC frames use a null-terminated ASCII media-type string.
        read_string_ignore_empty(&mut reader, Encoding::Iso8859_1)?
    };

    // Image usage.
    let usage = get_visual_key_from_picture_type(u32::from(reader.read_u8()?));

    let mut tags = vec![];

    // Null-teriminated image description in specified encoding.
    if let Some(desc) = read_string_ignore_empty(&mut reader, encoding)? {
        tags.push(Tag::new_from_parts("", "", Some(StandardTag::Description(Arc::from(desc)))));
    }

    // The remainder of the APIC frame is the image data. The frame has already been read into
    // memory in full, so this is bounded by the size of the tag.
    let data = Box::from(reader.read_buf_bytes_available_ref());

    // Try to get information about the image.
    let image_info = try_get_image_info(&data);

    let visual = visual_from_image_data(data, image_info, media_type, None, usage, tags);

    Ok(FrameResult::Visual(visual))
}

/// Reads an `ATXT` (audio encryption) frame.
pub fn read_atxt_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The encoding.
    let encoding = read_encoding(&mut reader)?;
    // The audio data mime-type.
    let mime_type = read_string(&mut reader, Encoding::Iso8859_1)?;
    // Single flag indicating if the audio data is scrambled.
    let is_scrambled = reader.read_u8()? & 1 != 0;
    // Text equivalent to what is spoken in the audio data.
    let equivalent_text = read_string(&mut reader, encoding)?;
    // The audio clip.
    let mut audio_data: Box<[u8]> = Box::from(reader.read_buf_bytes_available_ref());

    // Unscramble the audio data if it has been scrambled.
    if is_scrambled {
        let mut prbs = 0xfe;
        for byte in audio_data.iter_mut() {
            *byte ^= prbs;
            // Advance the pseudorandom binary sequence.
            prbs = (((prbs << 1) ^ (prbs << 2)) & 0xfc) | (((prbs >> 6) ^ (prbs >> 4)) & 0x3);
        }
    }

    let sub_fields = vec![
        RawTagSubField::new(ATXT_EQUIVALENT_TEXT, equivalent_text),
        RawTagSubField::new(ATXT_MIME_TYPE, mime_type),
    ];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, audio_data, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `CHAP` (chapter) frame.
pub fn read_chap_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    use crate::id3v2::frames::{
        min_frame_size, read_id3v2p2_frame, read_id3v2p3_frame, read_id3v2p4_frame,
    };

    // Read the element ID.
    let id = read_string(&mut reader, Encoding::Iso8859_1)?;

    // Start time in ms.
    let start_ms = reader.read_be_u32()?;
    // End time in ms.
    let end_ms = reader.read_be_u32()?;
    // Optional start position in bytes.
    let start_byte = match reader.read_be_u32()? {
        u32::MAX => None,
        start_byte => Some(u64::from(start_byte)),
    };
    // Optional end position in bytes.
    let end_byte = match reader.read_be_u32()? {
        u32::MAX => None,
        end_byte => Some(u64::from(end_byte)),
    };

    // Read supplemental tags.
    let mut tags = vec![];
    let mut visuals = vec![];

    while reader.bytes_available() >= min_frame_size(frame.major_version) {
        let frame = match frame.major_version {
            2 => read_id3v2p2_frame(&mut reader),
            3 => read_id3v2p3_frame(&mut reader),
            4 => read_id3v2p4_frame(&mut reader),
            _ => break,
        }?;

        match frame {
            FrameResult::MultipleTags(tag_list) => tags.extend(tag_list),
            FrameResult::Tag(mut tag) => {
                // The TIT2 (track title) and TIT3 (track subtitle/description) ID3v2 frames are
                // repurposed for chapter title and description, respectively.
                tag.std = match tag.std.take() {
                    Some(StandardTag::TrackTitle(title)) => Some(StandardTag::ChapterTitle(title)),
                    Some(StandardTag::TrackSubtitle(desc)) => Some(StandardTag::Description(desc)),
                    other => other,
                };

                tags.push(tag)
            }
            FrameResult::Visual(visual) => visuals.push(visual),
            _ => {}
        }
    }

    let chapter = Id3v2Chapter {
        id,
        read_order: 0,
        chapter: Chapter {
            start_time: Time::from_millis(i64::from(start_ms)),
            end_time: Some(Time::from_millis(i64::from(end_ms))),
            start_byte,
            end_byte,
            tags,
            visuals,
        },
    };

    Ok(FrameResult::Chapter(chapter))
}

/// Reads a `COMM` (comment) frame.
pub fn read_comm_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the description.
    let encoding = read_encoding(&mut reader)?;

    let mut sub_fields = Vec::new();
    let mut is_itunnorm = false;

    // The next three bytes are the language.
    if let Some(lang) = read_lang_code(&mut reader)? {
        sub_fields.push(RawTagSubField::new(COMM_LANGUAGE, lang));
    }

    // Optional content description.
    if let Some(desc) = read_string_ignore_empty(&mut reader, encoding)? {
        // A description of "iTunNORM" means the comment contains iTunes normalization data.
        is_itunnorm = desc == "iTunNORM";
        sub_fields.push(RawTagSubField::new(COMM_SHORT_DESCRIPTION, desc));
    }

    // The lyrics.
    let text = read_string(&mut reader, encoding)?;

    if is_itunnorm {
        return Ok(map_itunnorm(frame, text, sub_fields));
    }

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, text, sub_fields.into_boxed_slice());

    Ok(map_raw_tag(raw, Some(parse_comment)))
}

/// Maps an `iTunNORM` comment (iTunes Sound Check data) to a track gain and peak.
///
/// The comment is 10 space-separated 8-digit hexadecimal words (see
/// <https://id3.org/iTunes%20Normalization%20settings>). Words 0-1 are the left/right volume
/// adjustment relative to 1/1000 W, words 2-3 the same relative to 1/2500 W, and words 6-7 the
/// peak sample value (up to 32768). iTunes uses the largest of words 0-3 (words 2-3 rebased to
/// 1/1000 W) to adjust the track: `gain_dB = -10 * log10(max / 1000)`.
///
/// The comment itself is not a user comment, so it is only kept as an unmapped raw tag. If it can
/// not be parsed, only the raw tag is returned.
fn map_itunnorm(
    frame: &FrameInfo<'_>,
    text: String,
    sub_fields: Vec<RawTagSubField>,
) -> FrameResult {
    let words: Option<Vec<u32>> = text
        .split_ascii_whitespace()
        .map(|w| if w.len() == 8 { u32::from_str_radix(w, 16).ok() } else { None })
        .collect();

    let raw = Tag::new(RawTag::new_with_sub_fields(frame.id, text, sub_fields.into_boxed_slice()));

    let words = match words {
        Some(words) if words.len() == 10 => words,
        _ => return FrameResult::Tag(raw),
    };

    let mut tags: SmallVec<[Tag; 2]> = SmallVec::new();
    tags.push(raw);

    let max_a = f64::from(words[0].max(words[1]));
    // Rebase 1/2500 W values to 1/1000 W: -10*log10(v/2500) gain equals a 1000-based value of
    // v / 2.5.
    let max_b = f64::from(words[2].max(words[3])) / 2.5;
    let max = max_a.max(max_b);

    if max > 0.0 {
        let gain = format!("{:.2} dB", -10.0 * (max / 1000.0).log10());
        tags.push(Tag::new_from_parts(
            "REPLAYGAIN_TRACK_GAIN",
            gain.clone(),
            Some(StandardTag::ReplayGainTrackGain(Arc::new(gain))),
        ));
    }

    let peak_word = words[6].max(words[7]);

    if peak_word > 0 {
        let peak = format!("{:.6}", f64::from(peak_word) / 32768.0);
        tags.push(Tag::new_from_parts(
            "REPLAYGAIN_TRACK_PEAK",
            peak.clone(),
            Some(StandardTag::ReplayGainTrackPeak(Arc::new(peak))),
        ));
    }

    FrameResult::MultipleTags(tags)
}

/// Reads a `COMR` (commercial) frame.
pub fn read_comr_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the terms of use text.
    let encoding = read_encoding(&mut reader)?;

    // The price in the format: "<CURRENCY CODE 1><PRICE 1>[[/<CURRENCY CODE 2><PRICE 2>] ...]"
    let price = read_string(&mut reader, Encoding::Iso8859_1)?;

    let mut sub_fields = vec![
        // Price valid through this date.
        RawTagSubField::new(COMR_VALID_UNTIL, read_date(&mut reader)?),
        // Seller contact information (email address, URL, etc.)
        RawTagSubField::new(COMR_CONTACT_URL, read_string(&mut reader, Encoding::Iso8859_1)?),
        // How the audio was delivered. Takes one of the following values:
        // - 0x0  Other
        // - 0x1  Standard CD album with other songs
        // - 0x2  Compressed audio on CD
        // - 0x3  File over the Internet
        // - 0x4  Stream over the Internet
        // - 0x5  As note sheets
        // - 0x6  As note sheets in a book with other sheets
        // - 0x7  Music on other media
        // - 0x8  Non-musical merchandise
        RawTagSubField::new(COMR_RECEIVED_AS, reader.read_u8()?),
        // The seller name.
        RawTagSubField::new(COMR_SELLER_NAME, read_string(&mut reader, encoding)?),
        // The description of the product.
        RawTagSubField::new(COMR_DESCRIPTION, read_string(&mut reader, encoding)?),
    ];

    // Optional mime-type for the seller logo.
    let mime_type = read_string_ignore_empty(&mut reader, Encoding::Iso8859_1)?;
    // Optional seller logo.
    let logo = reader.read_buf_bytes_available_ref();

    if !logo.is_empty() {
        // Add the mime-type if it was provided.
        if let Some(mime_type) = mime_type {
            sub_fields.push(RawTagSubField::new(COMR_MIME_TYPE, mime_type));
        }
        // Add the seller logo data.
        sub_fields.push(RawTagSubField::new(COMR_SELLER_LOGO, logo));
    }

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, price, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `CRM` (encrypted meta) frame.
pub fn read_crm_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let owner = match read_string_ignore_empty(&mut reader, Encoding::Iso8859_1)? {
        Some(owner) => owner,
        None => {
            // Frame should be ignored if an owner is not specified.
            return Ok(FrameResult::Skipped);
        }
    };

    let desc = read_string_ignore_empty(&mut reader, Encoding::Iso8859_1)?;
    let data = reader.read_buf_bytes_available_ref();

    let mut sub_fields = vec![RawTagSubField::new(CRM_OWNER, owner)];

    if let Some(desc) = desc {
        sub_fields.push(RawTagSubField::new(CRM_DESCRIPTION, desc));
    }

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, data, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `CTOC` (table of contents) frame.
pub fn read_ctoc_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    use crate::id3v2::frames::{
        min_frame_size, read_id3v2p2_frame, read_id3v2p3_frame, read_id3v2p4_frame,
    };

    // Read for the element ID.
    let id = read_string(&mut reader, Encoding::Iso8859_1)?;

    // Read the flags.
    // - Bit 0 is the "ordered" bit. Indicates if the items should be played in order, or
    //   individually.
    // - Bit 1 is the "top-level" bit. Indicates if this table of contents is the root.
    let flags = reader.read_u8()?;
    // The number of items in this table of contents
    let entry_count = reader.read_u8()?;

    // Read child item element IDs.
    let mut items = Vec::with_capacity(usize::from(entry_count));

    for _ in 0..entry_count {
        let name = read_string(&mut reader, Encoding::Iso8859_1)?;
        items.push(name);
    }

    // Read supplemental tags.
    let mut tags = Vec::new();
    let mut visuals = Vec::new();

    while reader.bytes_available() >= min_frame_size(frame.major_version) {
        let frame = match frame.major_version {
            2 => read_id3v2p2_frame(&mut reader),
            3 => read_id3v2p3_frame(&mut reader),
            4 => read_id3v2p4_frame(&mut reader),
            _ => break,
        }?;

        match frame {
            FrameResult::MultipleTags(tag_list) => tags.extend(tag_list),
            FrameResult::Tag(tag) => tags.push(tag),
            FrameResult::Visual(visual) => visuals.push(visual),
            _ => {}
        }
    }

    let toc = Id3v2TableOfContents {
        id,
        top_level: flags & 2 != 0,
        ordered: flags & 1 != 0,
        items,
        tags,
        visuals,
    };

    Ok(FrameResult::TableOfContents(toc))
}

/// Reads an ENCR (encryption method registration) frame.
pub fn read_encr_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The owner identifier string.
    let owner = read_string(&mut reader, Encoding::Iso8859_1)?;
    // The encryption method symbol.
    let method = reader.read_u8()?;
    // The remainder of the frame is encryption method data.
    let encryption_data = reader.read_buf_bytes_available_ref();

    let sub_fields = vec![
        RawTagSubField::new(ENCR_OWNER, owner),
        RawTagSubField::new(ENCR_ENCRYPTION_DATA, encryption_data),
    ];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, method, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a GEOB (general encapsulated object) frame.
pub fn read_geob_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding.
    let encoding = read_encoding(&mut reader)?;
    // The mime-type. This is mandatory.
    let mime_type = read_string(&mut reader, Encoding::Iso8859_1)?;
    // Optional filename.
    let file_name = read_string_ignore_empty(&mut reader, encoding)?;
    // Optional content description.
    let desc = read_string_ignore_empty(&mut reader, encoding)?;
    // The object data.
    let object = reader.read_buf_bytes_available_ref();

    let mut sub_fields = Vec::new();

    sub_fields.push(RawTagSubField::new(GEOB_MIME_TYPE, mime_type));

    if let Some(file_name) = file_name {
        sub_fields.push(RawTagSubField::new(GEOB_FILE_NAME, file_name));
    }

    if let Some(desc) = desc {
        sub_fields.push(RawTagSubField::new(GEOB_DESCRIPTION, desc));
    }

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, object, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a GRID (group ID) frame.
pub fn read_grid_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The owner identifier string.
    let owner = read_string(&mut reader, Encoding::Iso8859_1)?;
    // The group ID/symbol.
    let group_id = reader.read_u8()?;
    // The remainder of the frame is group data.
    let group_data = reader.read_buf_bytes_available_ref();

    let sub_fields = vec![
        RawTagSubField::new(GRID_OWNER, owner),
        RawTagSubField::new(GRID_GROUP_DATA, group_data),
    ];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, group_id, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `MCDI` (music CD identifier) frame.
pub fn read_mcdi_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The entire frame is a binary dump of a CD-DA TOC (id3v2.4 section 4.5: "the binary dump of
    // a CD-DA table of contents").
    let buf = reader.read_buf_bytes_available_ref();

    // The standard tag is the uppercase hexadecimal encoding of the binary TOC. The raw value
    // retains the original binary data.
    let mut hex = String::with_capacity(buf.len() * 2);
    for byte in buf {
        hex.push_str(&format!("{byte:02X}"));
    }

    let std = if hex.is_empty() { None } else { Some(StandardTag::CdToc(Arc::new(hex))) };

    Ok(FrameResult::Tag(Tag::new_from_parts(frame.id, buf, std)))
}

/// Reads a timestamp format byte. Valid values are 1 (MPEG frames) and 2 (milliseconds).
fn read_timestamp_format(reader: &mut BufReader<'_>) -> Result<u8> {
    let format = reader.read_u8()?;

    if format != 1 && format != 2 {
        return decode_error("id3v2: invalid timestamp format");
    }

    Ok(format)
}

/// Reads a `SYLT` (synchronised lyrics/text) frame.
///
/// The value is the list of text segments. The timestamp of each segment is in the `TIMESTAMPS`
/// sub-field at the same index.
pub fn read_sylt_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let encoding = read_encoding(&mut reader)?;

    let mut sub_fields = Vec::new();

    if let Some(lang) = read_lang_code(&mut reader)? {
        sub_fields.push(RawTagSubField::new(SYLT_LANGUAGE, lang));
    }

    sub_fields
        .push(RawTagSubField::new(TIMING_TIMESTAMP_FORMAT, read_timestamp_format(&mut reader)?));
    // 0: other, 1: lyrics, 2: text transcription, 3: movement/part name, 4: events, 5: chord,
    // 6: trivia/pop-up information, 7: web page URLs, 8: image URLs.
    sub_fields.push(RawTagSubField::new(SYLT_CONTENT_TYPE, reader.read_u8()?));

    if let Some(desc) = read_string_ignore_empty(&mut reader, encoding)? {
        sub_fields.push(RawTagSubField::new(SYLT_DESCRIPTION, desc));
    }

    let mut texts = Vec::new();
    let mut times = Vec::new();

    while reader.bytes_available() > 0 {
        texts.push(read_string(&mut reader, encoding)?);
        times.push(reader.read_be_u32()?.to_string());
    }

    sub_fields.push(RawTagSubField::new(TIMING_TIMESTAMPS, times));

    let raw = RawTag::new_with_sub_fields(frame.id, texts, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads an `ETCO` (event timing codes) frame.
///
/// The value is the list of event types as decimal strings. The timestamp of each event is in the
/// `TIMESTAMPS` sub-field at the same index.
pub fn read_etco_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let format = read_timestamp_format(&mut reader)?;

    let mut events = Vec::new();
    let mut times = Vec::new();

    while reader.bytes_available() > 0 {
        events.push(reader.read_u8()?.to_string());
        times.push(reader.read_be_u32()?.to_string());
    }

    let sub_fields = vec![
        RawTagSubField::new(TIMING_TIMESTAMP_FORMAT, format),
        RawTagSubField::new(TIMING_TIMESTAMPS, times),
    ];

    let raw = RawTag::new_with_sub_fields(frame.id, events, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `SYTC` (synchronised tempo codes) frame.
///
/// The value is the list of tempos (BPM) as decimal strings. A tempo byte of 0xff is followed by a
/// second byte that is added to it. The timestamp of each tempo is in the `TIMESTAMPS` sub-field
/// at the same index.
pub fn read_sytc_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let format = read_timestamp_format(&mut reader)?;

    let mut tempos = Vec::new();
    let mut times = Vec::new();

    while reader.bytes_available() > 0 {
        let mut tempo = u16::from(reader.read_u8()?);

        if tempo == 0xff {
            tempo += u16::from(reader.read_u8()?);
        }

        tempos.push(tempo.to_string());
        times.push(reader.read_be_u32()?.to_string());
    }

    let sub_fields = vec![
        RawTagSubField::new(TIMING_TIMESTAMP_FORMAT, format),
        RawTagSubField::new(TIMING_TIMESTAMPS, times),
    ];

    let raw = RawTag::new_with_sub_fields(frame.id, tempos, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads an `EQU2` (equalisation 2) frame.
///
/// The value is the identification string. The adjustment points are in the `FREQUENCIES_HZ`
/// (stored in 1/2 Hz units) and `ADJUSTMENTS_DB` (stored in 1/512 dB units) sub-fields.
pub fn read_equ2_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // 0: band, 1: linear interpolation.
    let method = reader.read_u8()?;
    let ident = read_string(&mut reader, Encoding::Iso8859_1)?;

    let mut freqs = Vec::new();
    let mut adjusts = Vec::new();

    while reader.bytes_available() > 0 {
        let freq = reader.read_be_u16()?;
        let adjust = reader.read_be_u16()? as i16;

        freqs.push((f64::from(freq) / 2.0).to_string());
        adjusts.push((f64::from(adjust) / 512.0).to_string());
    }

    let sub_fields = vec![
        RawTagSubField::new(EQU2_INTERPOLATION_METHOD, method),
        RawTagSubField::new(EQU2_FREQUENCIES_HZ, freqs),
        RawTagSubField::new(EQU2_ADJUSTMENTS_DB, adjusts),
    ];

    let raw = RawTag::new_with_sub_fields(frame.id, ident, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `OWNE` (terms of use) frame.
pub fn read_owne_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the terms of use text.
    let encoding = read_encoding(&mut reader)?;
    // The price paid in the format: "<CURRENCY CODE><PRICE>".
    let price_paid = read_string(&mut reader, Encoding::Iso8859_1)?;

    let sub_fields = vec![
        // The date of purchase.
        RawTagSubField::new(OWNE_PURCHASE_DATE, read_date(&mut reader)?),
        // The name of the seller.
        RawTagSubField::new(OWNE_SELLER_NAME, read_string(&mut reader, encoding)?),
    ];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, price_paid, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `PCNT` (total file play count) frame.
pub fn read_pcnt_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // Read the mandatory play counter.
    let play_count = match read_play_counter(&mut reader)? {
        Some(count) => count,
        _ => return decode_error("pcnt: invalid play counter"),
    };

    // Create the tag.
    let tag = Tag::new_std(RawTag::new(frame.id, play_count), StandardTag::PlayCounter(play_count));

    Ok(FrameResult::Tag(tag))
}

/// Reads a `POPM` (popularimeter) frame.
pub fn read_popm_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let mut sub_fields = vec![
        // The email of the user this frame belongs to.
        RawTagSubField::new(POPM_EMAIL, read_string(&mut reader, Encoding::Iso8859_1)?),
    ];

    // Read the rating.
    let rating = reader.read_u8()?;

    // Read the optional play counter. Add it to the sub-fields of the tag.
    if let Some(play_counter) = read_play_counter(&mut reader)? {
        sub_fields.push(RawTagSubField::new(POPM_PLAY_COUNTER, play_counter));
    }

    // The primary value of this frame is the rating as it is mandatory whereas the play counter is
    // not. Add the user's email and play counter as sub-fields.
    let raw = RawTag::new_with_sub_fields(frame.id, rating, sub_fields.into_boxed_slice());

    // Create the tag. The rating byte (0-255) is mapped to a rating in PPM.
    let ppm = (u64::from(rating) * 2_000_000 + 255) / 510;
    let tag = Tag::new_std(raw, StandardTag::Rating(ppm as u32));

    Ok(FrameResult::Tag(tag))
}

/// Reads a `POSS` (position synchronisation) frame.
pub fn read_poss_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The units used for the position.
    let units = reader.read_u8()?;
    // The position.
    let position = reader.read_be_u32()?;

    let sub_fields = vec![RawTagSubField::new(POSS_POSITION_UNITS, units)];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, position, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads a `PRIV` (private) frame.
pub fn read_priv_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let sub_fields = vec![
        // Read the owner identifier.
        RawTagSubField::new(PRIV_OWNER, read_string(&mut reader, Encoding::Iso8859_1)?),
    ];

    // The remainder of the frame is binary data.
    let data = reader.read_buf_bytes_ref(reader.bytes_available() as usize)?;

    // Create the tag.
    let tag = Tag::new(RawTag::new_with_sub_fields(frame.id, data, sub_fields.into_boxed_slice()));

    Ok(FrameResult::Tag(tag))
}

/// Reads the body of a frame into a buffer, and return it as a tag with the frame ID as the key.
pub fn read_raw_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let data = reader.read_buf_bytes_available_ref();
    Ok(FrameResult::Tag(Tag::new_from_parts(frame.id, data, None)))
}

/// Reads a `SIGN` (signature) frame.
pub fn read_sign_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The group ID this signature belongs to.
    let group_id = reader.read_u8()?;
    // The remainder of the frame is the signature data.
    let signature = reader.read_buf_bytes_available_ref();

    let sub_fields = vec![RawTagSubField::new(SIGN_GROUP_ID, group_id)];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, signature, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(Tag::new(raw)))
}

/// Reads all text frames frame except for `TXXX`.
pub fn read_text_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding.
    let encoding = read_encoding(&mut reader)?;
    // Read 1 or more strings.
    let text = read_string_list(&mut reader, encoding)?;

    Ok(map_raw_tag(RawTag::new(frame.id, text), frame.raw_tag_parser))
}

/// Reads a `TIPL`/`IPLS` (involved people list) frame.
pub fn read_tipl_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding.
    let encoding = read_encoding(&mut reader)?;
    // Read 1 or more strings.
    let text = read_string_list(&mut reader, encoding)?;

    // The frame contains a list of pairs, where the first item in each pair is the function, and
    // the second item is the person (or persons) involved in that function. Pairs with a function
    // that has no standard tag are only kept in the raw tag.
    Ok(map_credit_pairs(RawTag::new(frame.id, text), |func| {
        TIPL_FUNC_PARSERS.get(func.to_ascii_lowercase().as_str()).copied()
    }))
}

/// Reads a `TMCL` (musician credits list) frame.
pub fn read_tmcl_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding.
    let encoding = read_encoding(&mut reader)?;
    // Read 1 or more strings.
    let text = read_string_list(&mut reader, encoding)?;

    // The frame contains a list of pairs, where the first item in each pair is the instrument (or
    // role), and the second item is the musician. Every musician is a performer. The role is
    // retained in the raw tag.
    Ok(map_credit_pairs(RawTag::new(frame.id, text), |_| Some(parse_performer as RawTagParser)))
}

/// Maps a raw tag containing a list of (role, person) pairs to standard tags. `parser_for_role`
/// returns the parser to apply to the person for a role, if any. If no pair can be mapped, or the
/// list is not a list of pairs, the raw tag is returned as-is.
fn map_credit_pairs(
    raw: RawTag,
    parser_for_role: impl Fn(&str) -> Option<RawTagParser>,
) -> FrameResult {
    let mut tags: SmallVec<[Tag; 2]> = Default::default();

    if let RawValue::StringList(list) = &raw.value {
        if list.len() % 2 == 0 {
            for pair in list.chunks_exact(2) {
                let person = pair[1].trim();

                if person.is_empty() {
                    continue;
                }

                if let Some(parser) = parser_for_role(pair[0].trim()) {
                    for std in parser(Arc::new(person.to_string())).into_iter().flatten() {
                        tags.push(Tag::new_std(raw.clone(), std));
                    }
                }
            }
        }
    }

    if tags.is_empty() { FrameResult::Tag(Tag::new(raw)) } else { FrameResult::MultipleTags(tags) }
}

/// Reads a `RVA2` (relative volume adjustment 2) frame. The master channel adjustment of a frame
/// identified as "track" or "album" is mapped to the ReplayGain gain and peak standard tags.
pub fn read_rva2_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    let data = reader.read_buf_bytes_available_ref();
    let raw = RawTag::new(frame.id, data);

    let mut tags: SmallVec<[Tag; 2]> = Default::default();

    if let Some((id, gain, peak)) = parse_rva2_master(data) {
        let gain = Arc::new(format!("{gain:.2} dB"));
        let peak = peak.map(|peak| Arc::new(format!("{peak:.6}")));

        let (gain_tag, peak_tag) = if id.eq_ignore_ascii_case("track") {
            (
                Some(StandardTag::ReplayGainTrackGain(gain)),
                peak.map(StandardTag::ReplayGainTrackPeak),
            )
        }
        else if id.eq_ignore_ascii_case("album") {
            (
                Some(StandardTag::ReplayGainAlbumGain(gain)),
                peak.map(StandardTag::ReplayGainAlbumPeak),
            )
        }
        else {
            (None, None)
        };

        for std in [gain_tag, peak_tag].into_iter().flatten() {
            tags.push(Tag::new_std(raw.clone(), std));
        }
    }

    if tags.is_empty() {
        Ok(FrameResult::Tag(Tag::new(raw)))
    }
    else {
        Ok(FrameResult::MultipleTags(tags))
    }
}

/// Parses the identification and master channel adjustment of a `RVA2` frame body. Returns the
/// identification, the adjustment in dB, and the peak normalised to full scale (if present).
fn parse_rva2_master(data: &[u8]) -> Option<(String, f64, Option<f64>)> {
    // The identification is a null-terminated ISO-8859-1 string.
    let end = data.iter().position(|&b| b == 0)?;
    let id: String = data[..end].iter().map(|&b| char::from(b)).collect();
    let mut rest = &data[end + 1..];

    // Each channel: type (1 byte), adjustment (i16, 1/512 dB), peak bits (1 byte), then the peak
    // volume in ceil(bits / 8) bytes.
    while rest.len() >= 4 {
        let channel_type = rest[0];
        let adjustment = i16::from_be_bytes([rest[1], rest[2]]);
        let peak_bytes = usize::from(rest[3]).div_ceil(8);

        let peak_data = rest.get(4..4 + peak_bytes)?;

        // Channel type 1 is the master volume.
        if channel_type == 1 {
            let peak = if peak_bytes > 0 && peak_bytes <= 8 {
                // The peak is a big-endian fixed-point value where full scale is half the range of
                // the bytes used to store it.
                let value = peak_data.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                Some(value as f64 / (1u64 << (8 * peak_bytes - 1)) as f64)
            }
            else {
                None
            };

            return Some((id, f64::from(adjustment) / 512.0, peak));
        }

        rest = &rest[4 + peak_bytes..];
    }

    None
}

/// Reads a `TXXX` (user defined) text frame.
pub fn read_txxx_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding.
    let encoding = read_encoding(&mut reader)?;
    // A description of the contents of the frame.
    let desc = read_string(&mut reader, encoding)?;
    // Read 1 or more strings.
    let text = read_string_list(&mut reader, encoding)?;

    // Some TXXX frames may be mapped to standard tags. Try to find a parser based on the
    // description string.
    let raw_tag_parser = TXXX_DESC_PARSERS.get(desc.to_ascii_lowercase().as_str()).copied();

    let sub_fields = vec![RawTagSubField::new(TXXX_DESCRIPTION, desc)];

    let raw = RawTag::new_with_sub_fields(frame.id, text, sub_fields.into_boxed_slice());

    Ok(map_raw_tag(raw, raw_tag_parser))
}

/// Readers an `UFID` (unique file identifier) frame.
pub fn read_ufid_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The owner identifier.
    let owner = read_string(&mut reader, Encoding::Iso8859_1)?;
    // An up-to 64-byte identifier fills the rest of the frame.
    let id = reader.read_buf_bytes_available_ref();

    // 64-bytes is the limit of the identifier.
    if id.len() > 64 {
        return decode_error("ufid: ufid identifier exceeds 64 bytes");
    }

    // The MusicBrainz recording ID is stored as an ASCII UUID string in a UFID frame owned by
    // "http://musicbrainz.org".
    let std = if owner.eq_ignore_ascii_case(UFID_OWNER_MUSICBRAINZ) {
        str::from_utf8(id)
            .ok()
            .map(|id| id.trim_end_matches('\0'))
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_graphic()))
            .map(|id| StandardTag::MusicBrainzRecordingId(Arc::new(id.to_string())))
    }
    else {
        None
    };

    let sub_fields = vec![RawTagSubField::new(UFID_OWNER, owner)];

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, id, sub_fields.into_boxed_slice());

    Ok(FrameResult::Tag(match std {
        Some(std) => Tag::new_std(raw, std),
        None => Tag::new(raw),
    }))
}

/// Reads all URL frames except for `WXXX`.
pub fn read_url_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // Read an URL string.
    let url = read_string(&mut reader, Encoding::Iso8859_1)?;

    // Create the tag.
    Ok(map_raw_tag(RawTag::new(frame.id, url), frame.raw_tag_parser))
}

/// Reads a USER (terms of use) frame.
pub fn read_user_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the terms of use text.
    let encoding = read_encoding(&mut reader)?;

    let mut sub_fields = Vec::new();

    // The next three bytes are an ISO-639-2 language code.
    if let Some(lang) = read_lang_code(&mut reader)? {
        sub_fields.push(RawTagSubField::new(USER_LANGUAGE, lang));
    }

    // Finally, the terms of use.
    let terms = read_string(&mut reader, encoding)?;

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, terms, sub_fields.into_boxed_slice());

    Ok(map_raw_tag(raw, Some(parse_terms_of_use)))
}

/// Reads a `USLT` (unsynchronized comment) frame.
pub fn read_uslt_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the frame is the encoding of the description.
    let encoding = read_encoding(&mut reader)?;

    let mut sub_fields = Vec::new();

    // The language code.
    if let Some(lang) = read_lang_code(&mut reader)? {
        sub_fields.push(RawTagSubField::new(USLT_LANGUAGE, lang));
    }

    // Optional content description.
    if let Some(desc) = read_string_ignore_empty(&mut reader, encoding)? {
        sub_fields.push(RawTagSubField::new(USLT_DESCRIPTION, desc));
    }

    // The lyrics.
    let lyrics = read_string(&mut reader, encoding)?;

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, lyrics, sub_fields.into_boxed_slice());

    Ok(map_raw_tag(raw, Some(parse_lyrics)))
}

/// Reads a `WXXX` (user defined) URL frame.
pub fn read_wxxx_frame(mut reader: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    // The first byte of the WXXX frame is the encoding of the description.
    let encoding = read_encoding(&mut reader)?;

    let sub_fields = vec![
        // Read the description string.
        RawTagSubField::new(WXXX_DESCRIPTION, read_string(&mut reader, encoding)?),
    ];

    // Read the ISO-8859-1 URL string.
    let url = read_string(&mut reader, Encoding::Iso8859_1)?;

    // Create the tag.
    let raw = RawTag::new_with_sub_fields(frame.id, url, sub_fields.into_boxed_slice());

    Ok(map_raw_tag(raw, frame.raw_tag_parser))
}

/// Skip the frame.
pub fn skip_frame(_: BufReader<'_>, frame: &FrameInfo<'_>) -> Result<FrameResult> {
    debug!("skipping '{}' frame", frame.id);
    Ok(FrameResult::Skipped)
}

/// Attempt to map a single-valued raw tag into zero or more standard tags, appending the resulting
/// tags to `tags`. If the value cannot be mapped, the raw tag is appended as-is.
fn map_single_raw_tag(raw: RawTag, parser: RawTagParser, tags: &mut SmallVec<[Tag; 2]>) {
    if let RawValue::String(value) = &raw.value {
        // Parse and return frame result.
        match parser(value.clone()) {
            [Some(std), None] | [None, Some(std)] => {
                // One raw tag yielded one standard tag.
                tags.push(Tag::new_std(raw, std));
                return;
            }
            [Some(std0), Some(std1)] => {
                // One raw tag yielded two standards tags.
                tags.push(Tag::new_std(raw.clone(), std0));
                tags.push(Tag::new_std(raw, std1));
                return;
            }
            // The raw value could not be parsed.
            _ => (),
        }
    }

    // Could not parse, add a raw tag.
    tags.push(Tag::new(raw));
}

/// Attempt to map the raw tag into one or more standard tags.
///
/// If the raw tag has multiple values (a list of strings) and a parser is provided, each value is
/// mapped individually, yielding one or more tags per value.
fn map_raw_tag(raw: RawTag, parser: Option<RawTagParser>) -> FrameResult {
    if let Some(parser) = parser {
        let mut tags: SmallVec<[Tag; 2]> = SmallVec::new();

        match &raw.value {
            RawValue::String(_) => {
                map_single_raw_tag(raw, parser, &mut tags);
            }
            RawValue::StringList(values) => {
                // A multi-valued frame. Map each non-empty value on its own.
                for value in values.iter().filter(|value| !value.is_empty()) {
                    let single = RawTag {
                        key: raw.key.clone(),
                        value: RawValue::from(value.clone()),
                        sub_fields: raw.sub_fields.clone(),
                    };

                    map_single_raw_tag(single, parser, &mut tags);
                }

                // If all values were empty, keep the original raw tag.
                if tags.is_empty() {
                    tags.push(Tag::new(raw));
                }
            }
            _ => tags.push(Tag::new(raw)),
        }

        return match tags.len() {
            1 => FrameResult::Tag(tags.remove(0)),
            _ => FrameResult::MultipleTags(tags),
        };
    }

    // No parser, add a raw tag.
    FrameResult::Tag(Tag::new(raw))
}

// Lookup tables.

// Mapping TXXX descriptions to raw tag parsers.
lazy_static! {
    static ref TXXX_DESC_PARSERS: RawTagParserMap = {
        let mut m: RawTagParserMap = HashMap::new();
        m.insert("acoustid fingerprint", parse_acoustid_fingerprint);
        m.insert("acoustid id", parse_acoustid_id);
        m.insert("albumartistsort", parse_sort_album_artist);
        m.insert("asin", parse_ident_asin);
        m.insert("barcode", parse_ident_barcode);
        m.insert("catalognumber", parse_ident_catalog_number);
        m.insert("albumsort", parse_sort_album);
        m.insert("artistsort", parse_sort_artist);
        m.insert("arranger", parse_arranger);
        m.insert("composersort", parse_sort_composer);
        m.insert("conductor", parse_conductor);
        m.insert("grouping", parse_grouping);
        m.insert("label", parse_label);
        m.insert("lyricist", parse_lyricist);
        m.insert("mood", parse_mood);
        m.insert("movementname", parse_movement_name);
        m.insert("itunesadvistory", parse_itunes_content_advisory);
        m.insert("license", parse_license);
        m.insert("musicbrainz album artist id", parse_musicbrainz_album_artist_id);
        m.insert("musicbrainz album id", parse_musicbrainz_album_id);
        m.insert("musicbrainz album release country", parse_release_country);
        m.insert("musicbrainz album status", parse_musicbrainz_release_status);
        m.insert("musicbrainz album type", parse_musicbrainz_release_type);
        m.insert("musicbrainz artist id", parse_musicbrainz_artist_id);
        m.insert("musicbrainz disc id", parse_musicbrainz_disc_id);
        m.insert("musicbrainz original album id", parse_musicbrainz_original_album_id);
        m.insert("musicbrainz original artist id", parse_musicbrainz_original_artist_id);
        m.insert("musicbrainz release group id", parse_musicbrainz_release_group_id);
        m.insert("musicbrainz release track id", parse_musicbrainz_release_track_id);
        m.insert("musicbrainz trm id", parse_musicbrainz_trm_id);
        m.insert("originaldate", parse_original_release_date);
        m.insert("originalyear", parse_original_release_year);
        m.insert("performer", parse_performer);
        m.insert("publisher", parse_label);
        m.insert("musicbrainz work id", parse_musicbrainz_work_id);
        m.insert("remixer", parse_remixer);
        m.insert("releasedate", parse_release_date);
        m.insert("replaygain_album_gain", parse_replaygain_album_gain);
        m.insert("replaygain_album_peak", parse_replaygain_album_peak);
        m.insert("replaygain_album_range", parse_replaygain_album_range);
        m.insert("replaygain_reference_loudness", parse_replaygain_reference_loudness);
        m.insert("replaygain_track_gain", parse_replaygain_track_gain);
        m.insert("replaygain_track_peak", parse_replaygain_track_peak);
        m.insert("replaygain_track_range", parse_replaygain_track_range);
        m.insert("script", parse_script);
        m.insert("subtitle", parse_track_subtitle);
        m.insert("titlesort", parse_sort_track_title);
        m.insert("work", parse_work);
        m.insert("writer", parse_writer);
        m
    };
}

// Mapping TIPL "functions" to raw tag parsers.
lazy_static! {
    static ref TIPL_FUNC_PARSERS: RawTagParserMap = {
        let mut m: RawTagParserMap = HashMap::new();
        m.insert("arranger", parse_arranger);
        m.insert("composer", parse_composer);
        m.insert("conductor", parse_conductor);
        m.insert("dj-mix", parse_mix_dj);
        m.insert("engineer", parse_engineer);
        m.insert("lyricist", parse_lyricist);
        m.insert("mix", parse_mix_engineer);
        m.insert("performer", parse_performer);
        m.insert("producer", parse_producer);
        m.insert("remixer", parse_remixer);
        m
    };
}

#[cfg(test)]
mod tests {
    use super::Encoding;

    use symphonia_core::io::BufReader;

    #[test]
    fn verify_read_date() {
        use super::read_date;

        // Empty buffer.
        assert!(read_date(&mut BufReader::new(&[])).is_err());
        // Too few characters in buffer.
        assert!(read_date(&mut BufReader::new(b"0")).is_err());
        assert!(read_date(&mut BufReader::new(b"01")).is_err());
        assert!(read_date(&mut BufReader::new(b"012")).is_err());
        assert!(read_date(&mut BufReader::new(b"0123")).is_err());
        assert!(read_date(&mut BufReader::new(b"01234")).is_err());
        assert!(read_date(&mut BufReader::new(b"012345")).is_err());
        assert!(read_date(&mut BufReader::new(b"0123456")).is_err());
        // Non-digit characters.
        assert!(read_date(&mut BufReader::new(b"0123456a")).is_err());
        assert!(read_date(&mut BufReader::new(b"abcdefgh")).is_err());
        // Exact number of digits.
        assert_eq!(read_date(&mut BufReader::new(b"20000101")).unwrap(), "20000101");
        // Read only 8 digits.
        assert_eq!(read_date(&mut BufReader::new(b"0123456789abcdef")).unwrap(), "01234567");
    }

    #[test]
    fn verify_read_encoding() {
        use super::read_encoding;

        // Empty buffer.
        assert!(read_encoding(&mut BufReader::new(&[])).is_err());
        // Various valid encodings.
        assert_eq!(read_encoding(&mut BufReader::new(&[0])).unwrap(), Encoding::Iso8859_1);
        assert_eq!(read_encoding(&mut BufReader::new(&[1])).unwrap(), Encoding::Utf16Bom);
        assert_eq!(read_encoding(&mut BufReader::new(&[2])).unwrap(), Encoding::Utf16Be);
        assert_eq!(read_encoding(&mut BufReader::new(&[3])).unwrap(), Encoding::Utf8);
        // Invalid encodings.
        assert!(read_encoding(&mut BufReader::new(&[4])).is_err());
        assert!(read_encoding(&mut BufReader::new(&[5])).is_err());
    }

    #[test]
    fn verify_read_lang_code() {
        use super::read_lang_code;

        // Empty buffer.
        assert!(read_lang_code(&mut BufReader::new(&[])).is_err());
        // Too few characters in buffer.
        assert!(read_lang_code(&mut BufReader::new(b"")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"e")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"en")).is_err());
        // Non-alphabetic.
        assert!(read_lang_code(&mut BufReader::new(b"0")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"01")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"012")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"en1")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"en!")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"   ")).is_err());
        assert!(read_lang_code(&mut BufReader::new(b"---")).is_err());
        // Valid language codes.
        assert_eq!(read_lang_code(&mut BufReader::new(b"enu")).unwrap().as_deref(), Some("enu"));
        assert_eq!(read_lang_code(&mut BufReader::new(b"jpn")).unwrap().as_deref(), Some("jpn"));
        // Valid language codes, mixed case.
        assert_eq!(read_lang_code(&mut BufReader::new(b"cHi")).unwrap().as_deref(), Some("chi"));
        assert_eq!(read_lang_code(&mut BufReader::new(b"IND")).unwrap().as_deref(), Some("ind"));
        // Unknown language codes.
        assert_eq!(read_lang_code(&mut BufReader::new(b"xxx")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"xxX")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"xXx")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"xXX")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"Xxx")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"XxX")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"XXx")).unwrap(), None);
        assert_eq!(read_lang_code(&mut BufReader::new(b"XXX")).unwrap(), None);
    }

    #[test]
    fn verify_read_play_counter() {
        use super::read_play_counter;

        // None.
        assert_eq!(read_play_counter(&mut BufReader::new(&[])).unwrap(), None);
        // Too small buffer.
        assert!(read_play_counter(&mut BufReader::new(&[0])).is_err());
        assert!(read_play_counter(&mut BufReader::new(&[0, 0])).is_err());
        assert!(read_play_counter(&mut BufReader::new(&[0, 0, 0])).is_err());
        // Valid.
        assert_eq!(read_play_counter(&mut BufReader::new(&[0, 0, 0, 0])).unwrap(), Some(0));
        assert_eq!(read_play_counter(&mut BufReader::new(&[0, 0, 0, 0, 0])).unwrap(), Some(0));
        assert_eq!(read_play_counter(&mut BufReader::new(&[0, 0, 0, 0, 0, 0])).unwrap(), Some(0));
        assert_eq!(
            read_play_counter(&mut BufReader::new(&[0, 0, 0, 0, 0, 0, 0])).unwrap(),
            Some(0)
        );
        assert_eq!(
            read_play_counter(&mut BufReader::new(&[0, 0, 0, 0, 0, 0, 0, 0])).unwrap(),
            Some(0)
        );
        // Too large.
        assert!(read_play_counter(&mut BufReader::new(&[0, 0, 0, 0, 0, 0, 0, 0, 0])).is_err());
        // Valid values.
        assert_eq!(
            read_play_counter(&mut BufReader::new(&u32::MAX.to_be_bytes())).unwrap(),
            Some(u64::from(u32::MAX))
        );
        assert_eq!(
            read_play_counter(&mut BufReader::new(&u64::MAX.to_be_bytes())).unwrap(),
            Some(u64::MAX)
        );
        assert_eq!(
            read_play_counter(&mut BufReader::new(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66])).unwrap(),
            Some(18_838_586_676_582)
        );
    }

    #[test]
    fn verify_read_string() {
        use super::read_string;

        // Empty strings.
        assert_eq!(read_string(&mut BufReader::new(&[]), Encoding::Utf8).unwrap(), "");
        // Null-terminated empty string.
        assert_eq!(read_string(&mut BufReader::new(b"\0"), Encoding::Utf8).unwrap(), "");
        // Non-terminated string.
        assert_eq!(
            read_string(&mut BufReader::new(b"Hello! 123!"), Encoding::Utf8).unwrap(),
            "Hello! 123!"
        );
        // Null-terminated string.
        assert_eq!(
            read_string(&mut BufReader::new(b"Null-terminated.\0"), Encoding::Utf8).unwrap(),
            "Null-terminated."
        );
        assert_eq!(
            read_string(&mut BufReader::new(b"Null-terminated.\0\0\0\0"), Encoding::Utf8).unwrap(),
            "Null-terminated."
        );
        // Two null-terminated strings.
        assert_eq!(
            read_string(&mut BufReader::new(b"Part 1\0Part 2\0"), Encoding::Utf8).unwrap(),
            "Part 1"
        );
    }

    #[test]
    fn verify_read_string_ignore_empty() {
        use super::read_string_ignore_empty;

        // Empty strings.
        assert_eq!(
            read_string_ignore_empty(&mut BufReader::new(&[]), Encoding::Utf8).unwrap(),
            None
        );
        // Null-terminated empty string.
        assert_eq!(
            read_string_ignore_empty(&mut BufReader::new(b"\0"), Encoding::Utf8).unwrap(),
            None
        );
    }

    #[test]
    fn verify_read_string_list() {
        use super::read_string_list;

        use std::sync::Arc;

        use symphonia_core::meta::RawValue;

        // Single item.
        assert_eq!(
            read_string_list(&mut BufReader::new(b""), Encoding::Utf8).unwrap(),
            RawValue::String(Arc::new("".into()))
        );
        assert_eq!(
            read_string_list(&mut BufReader::new(b"\0"), Encoding::Utf8).unwrap(),
            RawValue::String(Arc::new("".into()))
        );
        assert_eq!(
            read_string_list(&mut BufReader::new(b"Hello"), Encoding::Utf8).unwrap(),
            RawValue::String(Arc::new("Hello".into()))
        );
        assert_eq!(
            read_string_list(&mut BufReader::new(b"Hello\0"), Encoding::Utf8).unwrap(),
            RawValue::String(Arc::new("Hello".into()))
        );
        // Multiple items.
        assert_eq!(
            read_string_list(&mut BufReader::new(b"#1\0#2"), Encoding::Utf8).unwrap(),
            RawValue::StringList(Arc::new(vec!["#1".to_string(), "#2".to_string()]))
        );
        assert_eq!(
            read_string_list(&mut BufReader::new(b"#1\0#2\0"), Encoding::Utf8).unwrap(),
            RawValue::StringList(Arc::new(vec!["#1".to_string(), "#2".to_string()]))
        );
        // Empty items.
        assert_eq!(
            read_string_list(&mut BufReader::new(b"#1\0#2\0\0"), Encoding::Utf8).unwrap(),
            RawValue::StringList(Arc::new(vec![
                "#1".to_string(),
                "#2".to_string(),
                "".to_string()
            ]))
        );
        assert_eq!(
            read_string_list(&mut BufReader::new(b"#1\0#2\0\0\0"), Encoding::Utf8).unwrap(),
            RawValue::StringList(Arc::new(vec![
                "#1".to_string(),
                "#2".to_string(),
                "".to_string(),
                "".to_string()
            ]))
        );
        // All empty.
        assert_eq!(
            read_string_list(&mut BufReader::new(b"\0\0"), Encoding::Utf8).unwrap(),
            RawValue::StringList(Arc::new(vec!["".to_string(), "".to_string()]))
        );
    }

    /// ISO-8859-1 text decodes bytes 0x80-0x9f as Windows-1252.
    #[test]
    fn verify_decode_iso8859_1_windows_1252() {
        use super::decode_string_buf;

        assert_eq!(
            decode_string_buf(b"It\x92s a \x93test\x94", Encoding::Iso8859_1),
            "It\u{2019}s a \u{201c}test\u{201d}"
        );
        assert_eq!(
            decode_string_buf(b"\x80 \x96 \x97 \x99", Encoding::Iso8859_1),
            "\u{20ac} \u{2013} \u{2014} \u{2122}"
        );
        // The five code points undefined in Windows-1252.
        assert_eq!(
            decode_string_buf(b"\x81\x8d\x8f\x90\x9d", Encoding::Iso8859_1),
            "\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}"
        );
        // C0 control codes other than line-feed are still replaced, Latin-1 is unchanged.
        assert_eq!(decode_string_buf(b"a\x01\nb\xe9", Encoding::Iso8859_1), "a\u{fffd}\nb\u{e9}");
    }
}
