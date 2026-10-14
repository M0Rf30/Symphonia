// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Image utilities.

use std::num::NonZeroU8;

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BufReader, ReadBytes};
use symphonia_core::meta::{
    ColorMode, ColorModel, ColorPaletteInfo, Size, StandardVisualKey, Tag, Visual,
};

use log::debug;

/// Image information.
#[derive(Clone, Debug)]
pub struct ImageInfo {
    /// The Media Type (MIME Type) of the image format.
    pub media_type: String,
    /// The dimensions of the image.
    pub dimensions: Size,
    /// The color mode of the image.
    pub color_mode: ColorMode,
}

/// Create a NonZeroU8 or panic.
const fn non_zero(value: u8) -> NonZeroU8 {
    match NonZeroU8::new(value) {
        Some(nz) => nz,
        None => panic!(),
    }
}

/// Try to get basic information about an image from an image buffer.
pub fn try_get_image_info(buf: &[u8]) -> Option<ImageInfo> {
    struct Parser {
        parse: for<'a> fn(BufReader<'a>) -> Result<ImageInfo>,
        marker: &'static [u8],
    }

    const IMAGE_PARSERS: &[Parser] = &[
        Parser { marker: &[0x42, 0x4d], parse: parse_bitmap },
        Parser { marker: &[0xff, 0xd8], parse: parse_jpeg },
        Parser { marker: &[0x47, 0x49, 0x46, 0x38, 0x37, 0x61], parse: parse_gif },
        Parser { marker: &[0x47, 0x49, 0x46, 0x38, 0x39, 0x61], parse: parse_gif },
        Parser { marker: &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], parse: parse_png },
        Parser { marker: b"RIFF", parse: parse_webp },
    ];

    debug!("detecting format of image starting with: {:02x?}", &buf[..8.min(buf.len())]);

    // Find the first image parser that has a marker that matches the beginning of the image
    // buffer, and attempt to parse it for image information.
    IMAGE_PARSERS
        .iter()
        .filter(|parser| buf.starts_with(parser.marker))
        .find_map(|parser| (parser.parse)(BufReader::new(&buf[parser.marker.len()..])).ok())
}

/// Create a visual from image data and the image information detected from it, if any.
///
/// The detected image information is preferred over the declared media type and dimensions, which
/// are used only if the image information is unavailable.
pub fn visual_from_image_data(
    data: Box<[u8]>,
    info: Option<ImageInfo>,
    declared_media_type: Option<String>,
    declared_dimensions: Option<Size>,
    usage: Option<StandardVisualKey>,
    tags: Vec<Tag>,
) -> Visual {
    let (media_type, dimensions, color_mode) = match info {
        Some(info) => (Some(info.media_type), Some(info.dimensions), Some(info.color_mode)),
        None => (declared_media_type, declared_dimensions, None),
    };

    Visual { media_type, dimensions, color_mode, usage, tags, data }
}

/// Parse a JPEG for image information.
fn parse_jpeg(mut reader: BufReader<'_>) -> Result<ImageInfo> {
    while reader.read_u8()? == 0xff {
        let chunk_type = reader.read_u8()?;

        // Skip parameter-less markers, see https://github.com/corkami/formats/blob/master/image/jpeg.md
        if chunk_type >= 0xd0 && chunk_type <= 0xd9 {
            continue;
        }

        let chunk_len = reader.read_be_u16()?;
        if chunk_len < 2 {
            return decode_error("meta (jpeg): invalid chunk length");
        }

        // Baseline, and progressive DCT.
        if chunk_type == 0xc0 || chunk_type == 0xc2 {
            let _ = reader.read_u8()?;
            let height = reader.read_be_u16()?;
            let width = reader.read_be_u16()?;

            let color_mode = ColorMode::Direct(ColorModel::RGB(non_zero(8)));

            let info = ImageInfo {
                media_type: "image/jpeg".to_string(),
                dimensions: Size { width: u32::from(width), height: u32::from(height) },
                color_mode,
            };

            return Ok(info);
        }

        // Ignore the chunk. Exclude the chunk length that has already been read.
        reader.ignore_bytes(u64::from(chunk_len) - 2)?;
    }

    decode_error("meta (jpeg): invalid data")
}

/// Parse a PNG for image information.
fn parse_png(mut reader: BufReader<'_>) -> Result<ImageInfo> {
    // A PNG must start with an IHDR chunk.
    reader.ignore_bytes(4)?;
    if reader.read_quad_bytes()? != *b"IHDR" {
        return decode_error("meta (png): invalid data");
    }

    let width = reader.read_be_u32()?;
    let height = reader.read_be_u32()?;
    let bit_depth = reader.read_u8()?;
    let color_type = reader.read_u8()?;
    // Ignore compression method, filter method, interlace method, and CRC to reach the end of
    // the IHDR chunk.
    reader.ignore_bytes(7)?;

    let bit_depth = match NonZeroU8::new(bit_depth) {
        Some(bit_depth) => bit_depth,
        _ => return decode_error("meta (png): bit depth cannot be 0"),
    };

    // Only certain bit-depths are supported based on the color type.
    let color_mode = match color_type {
        // Greyscale (0)
        0 if [1, 2, 4, 8, 16].contains(&bit_depth.get()) => {
            ColorMode::Direct(ColorModel::Y(bit_depth))
        }
        // Truecolor (2)
        2 if [8, 16].contains(&bit_depth.get()) => ColorMode::Direct(ColorModel::RGB(bit_depth)),
        // Greyscale with alpha (4)
        4 if [8, 16].contains(&bit_depth.get()) => ColorMode::Direct(ColorModel::YA(bit_depth)),
        // Truecolor with alpha (6)
        6 if [8, 16].contains(&bit_depth.get()) => ColorMode::Direct(ColorModel::RGBA(bit_depth)),
        // Indexed (3)
        3 if [1, 2, 4, 8].contains(&bit_depth.get()) => {
            // Check if there is an alpha channel.
            let color_model = loop {
                let chunk_len = reader.read_be_u32()?;
                let chunk_type = reader.read_quad_bytes()?;

                // The tRNS chunk has been found, the palette contains an alpha channel.
                if chunk_type == *b"tRNS" {
                    // 8-bit RGBA.
                    break ColorModel::RGBA(non_zero(8));
                }

                // The tRNS chunk must appear before IDAT, and definitely before IEND. If it
                // doesn't, then the palette doesn't contain an alpha channel.
                if chunk_type == *b"IDAT" || chunk_type == *b"IEND" {
                    // 8-bit RGB.
                    break ColorModel::RGB(non_zero(8));
                }

                // Skip chunk data and CRC.
                reader.ignore_bytes(u64::from(chunk_len) + 4)?;
            };

            ColorMode::Indexed(ColorPaletteInfo {
                bits_per_pixel: non_zero(bit_depth.get()),
                color_model,
            })
        }
        _ => return decode_error("meta (png): invalid color type and bit depth combination"),
    };

    let info = ImageInfo {
        media_type: "image/png".to_string(),
        dimensions: Size { width, height },
        color_mode,
    };

    Ok(info)
}

/// Parse a Bitmap for image information.
fn parse_bitmap(mut reader: BufReader<'_>) -> Result<ImageInfo> {
    // Ignore the BITMAPFILEHEADER contents after the signature.
    reader.ignore_bytes(12)?;

    // The header size differentiates the version/type of bitmap header.
    let size = reader.read_u32()?;

    // Support the 5 versions of the Windows BITMAPINFOHEADER. Each subsequent version is an
    // incremental extension of the previous, however, we only are concerned with the fields
    // from the base header.
    if ![40, 52, 56, 108, 124].contains(&size) {
        return decode_error("meta (bmp): unsupported bitmap header");
    }

    // Width should always be positive.
    let width = reader.read_i32()?.unsigned_abs();
    // Height can be negative to indicate a top-down bitmap instead of a bottom-up bitmap. This
    // makes no difference to the actual size.
    let height = reader.read_i32()?.unsigned_abs();
    // The number of color planes should always be 1.
    let planes = reader.read_u16()?;

    if planes != 1 {
        return decode_error("meta (bmp): invalid number of planes");
    }

    let bit_count = reader.read_u16()?;
    let compression = reader.read_u32()?;

    // reader.ignore_bytes(4)?;

    let color_mode = match compression {
        // BI_RGB = 0x0 (RGB uncompressed)
        0 => {
            if bit_count > 0 && bit_count <= 8 {
                // If the bit count is <= 8, a color table is present. Each color is stored as
                // a RGBQUAD (R8G8B8).
                ColorMode::Indexed(ColorPaletteInfo {
                    bits_per_pixel: non_zero(bit_count as u8),
                    color_model: ColorModel::RGB(non_zero(8)),
                })
            }
            else if bit_count == 16 {
                // With 16bpp, R5G5B5 is used.
                ColorMode::Direct(ColorModel::RGB(non_zero(5)))
            }
            else if bit_count == 24 || bit_count == 32 {
                // With 24bpp or 32bpp, R8G8B8(A8) is used.
                ColorMode::Direct(ColorModel::RGB(non_zero(8)))
            }
            else {
                return decode_error("meta (bmp): invalid bit count");
            }
        }
        // BI_RLE8 = 0x1 (run-length encoded)
        1 => {
            // Run-length encoded pixel values storing indices into the RGBQUAD color table.
            // Pixel data is stored as a (count, index) byte-pair.
            ColorMode::Indexed(ColorPaletteInfo {
                bits_per_pixel: non_zero(8),
                color_model: ColorModel::RGB(non_zero(8)),
            })
        }
        // BI_RLE4 = 0x2 (run-length encoded)
        2 => {
            // Run-length encoded pixel values storing indices into the RGBQUAD color table.
            // Pixel data is stored as a (count, index[0] | index[1]) byte-pair.
            ColorMode::Indexed(ColorPaletteInfo {
                bits_per_pixel: non_zero(4),
                color_model: ColorModel::RGB(non_zero(8)),
            })
        }
        // BI_BITFIELDS = 0x3 (color masks)
        // BI_JPEG = 0x4 (JPEG)
        // BI_PNG = 0x5 (PNG)
        // BI_CMYK = 0xB (CMYK uncompressed)
        // BI_CMYKRLE8 = 0xC (CMYK run-length encoded)
        // BI_CMYKRLE4 = 0xD (CMYK run-length encoded)
        _ => return unsupported_error("meta (bmp): compression is unsupported"),
    };

    let info = ImageInfo {
        media_type: "image/bmp".to_string(),
        dimensions: Size { width, height },
        color_mode,
    };

    Ok(info)
}

/// Parse a GIF for image information.
fn parse_gif(mut reader: BufReader<'_>) -> Result<ImageInfo> {
    let width = reader.read_u16()?;
    let height = reader.read_u16()?;
    // Flags specify if a global color table (GCT) is used, and its size.
    let flags = reader.read_u8()?;

    let color_mode = if flags & 0x80 != 0 {
        // GCT is enabled. The lower flag bits indicate the bits per pixel.
        let bpp = (flags & 0x7) + 1;

        ColorMode::Indexed(ColorPaletteInfo {
            bits_per_pixel: non_zero(bpp),
            color_model: ColorModel::RGB(non_zero(8)),
        })
    }
    else {
        // No GCT.
        return unsupported_error("meta (gif): local color tables are unsupported");
    };

    let info = ImageInfo {
        media_type: "image/gif".to_string(),
        dimensions: Size { width: u32::from(width), height: u32::from(height) },
        color_mode,
    };

    Ok(info)
}

/// Parse a WebP for image information. The layout is specified by RFC 9649.
fn parse_webp(mut reader: BufReader<'_>) -> Result<ImageInfo> {
    // The RIFF size, then the WEBP form type.
    reader.ignore_bytes(4)?;
    if reader.read_quad_bytes()? != *b"WEBP" {
        return decode_error("meta (webp): not a webp image");
    }

    let chunk_id = reader.read_quad_bytes()?;
    reader.ignore_bytes(4)?;

    let (width, height, alpha) = match &chunk_id {
        // Simple lossy: a VP8 frame tag (3 bytes), start code, then 14-bit width and height
        // (RFC 6386 section 9.1). Lossy images without a VP8X header have no alpha.
        b"VP8 " => {
            reader.ignore_bytes(3)?;
            if reader.read_triple_bytes()? != [0x9d, 0x01, 0x2a] {
                return decode_error("meta (webp): invalid vp8 start code");
            }
            let width = reader.read_u16()? & 0x3fff;
            let height = reader.read_u16()? & 0x3fff;
            (u32::from(width), u32::from(height), false)
        }
        // Simple lossless: signature, then 14-bit width - 1, 14-bit height - 1, and alpha_is_used.
        b"VP8L" => {
            if reader.read_u8()? != 0x2f {
                return decode_error("meta (webp): invalid vp8l signature");
            }
            let bits = reader.read_u32()?;
            ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1, bits & (1 << 28) != 0)
        }
        // Extended: flags (alpha is bit 4), 3 reserved bytes, then 24-bit canvas width - 1 and
        // height - 1.
        b"VP8X" => {
            let flags = reader.read_u8()?;
            reader.ignore_bytes(3)?;
            let width = reader.read_u24()? + 1;
            let height = reader.read_u24()? + 1;
            (width, height, flags & 0x10 != 0)
        }
        _ => return decode_error("meta (webp): unknown first chunk"),
    };

    let color_model =
        if alpha { ColorModel::RGBA(non_zero(8)) } else { ColorModel::RGB(non_zero(8)) };

    Ok(ImageInfo {
        media_type: "image/webp".to_string(),
        dimensions: Size { width, height },
        color_mode: ColorMode::Direct(color_model),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_png_with_chunk_len(chunk_len: u32) -> Vec<u8> {
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&[8, 3, 0, 0, 0]);
        png.extend_from_slice(&[0; 4]);
        png.extend_from_slice(&chunk_len.to_be_bytes());
        png.extend_from_slice(b"PLTE");
        png.extend_from_slice(&[0; 16]);
        png
    }

    #[test]
    fn verify_png_palette_chunk_length_does_not_overflow() {
        for chunk_len in [u32::MAX - 4, u32::MAX - 3, u32::MAX - 1, u32::MAX] {
            assert!(try_get_image_info(&indexed_png_with_chunk_len(chunk_len)).is_none());
        }
    }

    #[test]
    fn verify_png_palette_chunk_is_skipped() {
        let mut png = indexed_png_with_chunk_len(3);
        png.truncate(png.len() - 16);
        png.extend_from_slice(&[0; 3 + 4]);
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IDAT");

        let info = try_get_image_info(&png).unwrap();
        assert_eq!(info.media_type, "image/png");
    }

    fn webp(chunk_id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(12 + payload.len() as u32).to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(chunk_id);
        webp.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        webp.extend_from_slice(payload);
        webp
    }

    fn webp_info(chunk_id: &[u8; 4], payload: &[u8]) -> (u32, u32, ColorMode) {
        let info = try_get_image_info(&webp(chunk_id, payload)).unwrap();
        assert_eq!(info.media_type, "image/webp");
        (info.dimensions.width, info.dimensions.height, info.color_mode)
    }

    #[test]
    fn verify_webp_dimensions_and_alpha() {
        let rgb = ColorMode::Direct(ColorModel::RGB(non_zero(8)));
        let rgba = ColorMode::Direct(ColorModel::RGBA(non_zero(8)));

        // Lossy 320x240; the top two bits of each dimension are the scale and are ignored.
        let vp8 = [0, 0, 0, 0x9d, 0x01, 0x2a, 0x40, 0x41, 0xf0, 0xc0];
        assert_eq!(webp_info(b"VP8 ", &vp8), (320, 240, rgb));

        // Lossless 16383x2 with alpha: width - 1 = 0x3ffe, height - 1 = 1, alpha bit 28.
        let bits: u32 = 0x3ffe | (1 << 14) | (1 << 28);
        let mut vp8l = vec![0x2f];
        vp8l.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(webp_info(b"VP8L", &vp8l), (16383, 2, rgba));

        // Extended 1x16777216 canvas, without and with the alpha flag.
        let vp8x = [0x00, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff];
        assert_eq!(webp_info(b"VP8X", &vp8x), (1, 16_777_216, rgb));
        let vp8x = [0x10, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff];
        assert_eq!(webp_info(b"VP8X", &vp8x), (1, 16_777_216, rgba));
    }

    #[test]
    fn verify_riff_that_is_not_webp_is_rejected() {
        let mut wav = webp(b"VP8X", &[0; 10]);
        wav[8..12].copy_from_slice(b"WAVE");
        assert!(try_get_image_info(&wav).is_none());
    }
}
