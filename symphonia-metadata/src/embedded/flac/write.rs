// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FLAC metadata block writing.

use symphonia_core::errors::{Error, Result};
use symphonia_core::meta::{ColorMode, RawValue, StandardTag, Visual};

use crate::utils::id3v2::get_picture_type_from_visual_key;
use crate::utils::images::try_get_image_info;

/// The `STREAMINFO` metadata block type.
pub const BLOCK_TYPE_STREAMINFO: u8 = 0;
/// The `PADDING` metadata block type.
pub const BLOCK_TYPE_PADDING: u8 = 1;
/// The `APPLICATION` metadata block type.
pub const BLOCK_TYPE_APPLICATION: u8 = 2;
/// The `SEEKTABLE` metadata block type.
pub const BLOCK_TYPE_SEEKTABLE: u8 = 3;
/// The `VORBIS_COMMENT` metadata block type.
pub const BLOCK_TYPE_VORBIS_COMMENT: u8 = 4;
/// The `CUESHEET` metadata block type.
pub const BLOCK_TYPE_CUESHEET: u8 = 5;
/// The `PICTURE` metadata block type.
pub const BLOCK_TYPE_PICTURE: u8 = 6;

/// The maximum length of a metadata block body in bytes (24-bit length field).
pub const MAX_BLOCK_LEN: usize = 0x00ff_ffff;

/// Builds the 4 byte header of a metadata block with a body of `len` bytes.
pub fn block_header(is_last: bool, block_type: u8, len: usize) -> Result<[u8; 4]> {
    if len > MAX_BLOCK_LEN {
        return Err(Error::Unsupported("meta (flac): metadata block exceeds 16 MiB"));
    }

    let len = (len as u32).to_be_bytes();

    Ok([(u8::from(is_last) << 7) | (block_type & 0x7f), len[1], len[2], len[3]])
}

/// Serializes the body of a `PICTURE` metadata block (which is also the content of a
/// `METADATA_BLOCK_PICTURE` Vorbis comment, once base64 encoded).
///
/// The media type, dimensions, and colour depth are taken from the visual when it has them and
/// otherwise detected from the image data. The description is the visual's `DESCRIPTION` tag.
pub fn picture_block(visual: &Visual) -> Result<Vec<u8>> {
    let info = try_get_image_info(&visual.data);

    let media_type = visual
        .media_type
        .clone()
        .or_else(|| info.as_ref().map(|info| info.media_type.clone()))
        .unwrap_or_default();

    let dimensions = visual.dimensions.or_else(|| info.as_ref().map(|info| info.dimensions));
    let color_mode = visual.color_mode.or_else(|| info.as_ref().map(|info| info.color_mode));

    let (depth, colors) = match color_mode {
        Some(ColorMode::Direct(model)) => (model.bits_per_pixel(), 0),
        Some(ColorMode::Indexed(palette)) => {
            let bits = u32::from(palette.bits_per_pixel.get());
            (bits, 1u32.checked_shl(bits).unwrap_or(0))
        }
        None => (0, 0),
    };

    let description = visual
        .tags
        .iter()
        .find_map(|tag| match (&tag.std, &tag.raw.value) {
            (Some(StandardTag::Description(desc)), _) => Some(desc.as_str()),
            (None, RawValue::String(desc)) if tag.raw.key.eq_ignore_ascii_case("DESCRIPTION") => {
                Some(desc.as_str())
            }
            _ => None,
        })
        .unwrap_or_default();

    let too_large = || Error::Unsupported("meta (flac): picture exceeds 4 GiB");
    let len = |len: usize| u32::try_from(len).map_err(|_| too_large());

    let mut out = Vec::with_capacity(32 + media_type.len() + description.len() + visual.data.len());

    out.extend_from_slice(&u32::from(get_picture_type_from_visual_key(visual.usage)).to_be_bytes());
    out.extend_from_slice(&len(media_type.len())?.to_be_bytes());
    out.extend_from_slice(media_type.as_bytes());
    out.extend_from_slice(&len(description.len())?.to_be_bytes());
    out.extend_from_slice(description.as_bytes());
    out.extend_from_slice(&dimensions.map_or(0, |d| d.width).to_be_bytes());
    out.extend_from_slice(&dimensions.map_or(0, |d| d.height).to_be_bytes());
    out.extend_from_slice(&depth.to_be_bytes());
    out.extend_from_slice(&colors.to_be_bytes());
    out.extend_from_slice(&len(visual.data.len())?.to_be_bytes());
    out.extend_from_slice(&visual.data);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use symphonia_core::io::BufReader;
    use symphonia_core::meta::{RawTag, StandardVisualKey, Tag};

    use super::*;
    use crate::embedded::flac::read_flac_picture_block;

    // A 1x1 GIF.
    const GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
    ];

    #[test]
    fn verify_block_header() {
        assert_eq!(block_header(true, 4, 0x010203).unwrap(), [0x84, 0x01, 0x02, 0x03]);
        assert_eq!(block_header(false, 6, 5).unwrap(), [0x06, 0, 0, 5]);
        assert!(block_header(false, 1, MAX_BLOCK_LEN + 1).is_err());
    }

    #[test]
    fn verify_picture_round_trip() {
        let desc = Arc::new("Front".to_string());

        let visual = Visual {
            media_type: None,
            dimensions: None,
            color_mode: None,
            usage: Some(StandardVisualKey::BackCover),
            tags: vec![Tag::new_std(
                RawTag::new("DESCRIPTION", desc.clone()),
                StandardTag::Description(desc),
            )],
            data: GIF.into(),
        };

        let block = picture_block(&visual).unwrap();
        let read = read_flac_picture_block(&mut BufReader::new(&block)).unwrap();

        assert_eq!(read.media_type.as_deref(), Some("image/gif"));
        assert_eq!(read.usage, Some(StandardVisualKey::BackCover));
        assert_eq!(&*read.data, GIF);
        assert_eq!(read.tags.len(), 1);
        assert_eq!(read.dimensions.map(|d| (d.width, d.height)), Some((1, 1)));
    }
}
