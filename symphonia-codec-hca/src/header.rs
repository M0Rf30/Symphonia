// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The stream header (`init_hca` of FFmpeg's `libavcodec/hcadec.c`).
//!
//! The header is a sequence of chunks, each introduced by a four character tag whose bytes may
//! have their top bit set (`HCA_MASK`):
//!
//! ```text
//! 0x00  "HCA\0", u16 version, u16 header size
//! 0x08  "fmt\0", u8 channels, u24 sample rate, u32 blocks, u16 mute header, u16 mute footer
//!       "comp" (or "dec\0"): the block size and the band layout
//!       "vbr\0", "ath\0", "loop", "ciph", "rva\0", "comm", "pad\0": optional
//! ```
//!
//! All fields are big-endian.

use symphonia_core::errors::{Result, decode_error};

/// The largest number of channels (`MAX_CHANNELS`).
pub(crate) const MAX_CHANNELS: usize = 16;

/// The number of spectral lines of a subframe, and the largest number of bands.
pub(crate) const BANDS: usize = 128;

/// Clears the top bit of the bytes of a chunk tag (`HCA_MASK`).
fn tag(bytes: [u8; 4]) -> [u8; 4] {
    bytes.map(|b| b & 0x7f)
}

/// A reader of big-endian fields of the header.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let Some(bytes) = self.data.get(self.pos..self.pos + len)
        else {
            return decode_error("hca: truncated header");
        };
        self.pos += len;
        Ok(bytes)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    fn tag(&mut self) -> Result<[u8; 4]> {
        let b = self.take(4)?;
        Ok(tag([b[0], b[1], b[2], b[3]]))
    }

    fn left(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// The fields of the header that the decoder uses.
#[derive(Clone, Debug)]
pub(crate) struct Header {
    pub channels: usize,
    pub sample_rate: u32,
    /// The size in bytes of a frame.
    pub block_size: u16,
    pub track_count: usize,
    pub channel_config: u8,
    pub total_band_count: usize,
    pub base_band_count: usize,
    pub stereo_band_count: usize,
    pub bands_per_hfr_group: usize,
    /// The number of groups of high frequency bands.
    pub hfr_group_count: usize,
    /// 0: no absolute threshold of hearing table, 1: the table of the sample rate.
    pub ath_type: u16,
}

impl Header {
    /// Parses a header. `data` is the header alone, `header size` bytes.
    pub fn parse(data: &[u8]) -> Result<Header> {
        let mut gb = Cursor { data, pos: 0 };

        if gb.tag()? != *b"HCA\0" {
            return decode_error("hca: invalid signature");
        }

        let version = gb.u16()?;
        gb.skip(2)?;

        // Streams of version 2.0 or later carry no ATH table by default.
        let mut ath_type = if version >= 0x200 { 0 } else { 1 };

        if gb.tag()? != *b"fmt\0" {
            return decode_error("hca: missing fmt chunk");
        }

        let channels = usize::from(gb.u8()?);
        let sample_rate = gb.u24()?;
        // The number of blocks and the muted samples are for the demuxer.
        gb.skip(8)?;

        if channels == 0 || channels > MAX_CHANNELS {
            return decode_error("hca: invalid channel count");
        }

        if sample_rate == 0 {
            return decode_error("hca: invalid sample rate");
        }

        let (block_size, track_count, channel_config);
        let (total_band_count, base_band_count, stereo_band_count, bands_per_hfr_group);

        match &gb.tag()? {
            b"comp" => {
                block_size = gb.u16()?;
                // The minimum and the maximum resolution.
                gb.skip(2)?;
                track_count = gb.u8()?;
                channel_config = gb.u8()?;
                total_band_count = usize::from(gb.u8()?);
                base_band_count = usize::from(gb.u8()?);
                stereo_band_count = usize::from(gb.u8()?);
                bands_per_hfr_group = usize::from(gb.u8()?);
                gb.skip(2)?;
            }
            b"dec\0" => {
                block_size = gb.u16()?;
                gb.skip(2)?;
                total_band_count = usize::from(gb.u8()?) + 1;
                let base = usize::from(gb.u8()?) + 1;
                let tracks = gb.u8()?;
                track_count = tracks >> 4;
                channel_config = tracks & 0xf;
                base_band_count = if gb.u8()? == 0 { total_band_count } else { base };
                stereo_band_count = total_band_count.saturating_sub(base_band_count);
                bands_per_hfr_group = 0;
            }
            _ => return decode_error("hca: missing comp or dec chunk"),
        }

        if block_size < 8 {
            return decode_error("hca: invalid block size");
        }

        if total_band_count > BANDS {
            return decode_error("hca: invalid band count");
        }

        // The optional chunks end at the first one that is not known.
        while gb.left() >= 4 {
            match &gb.tag()? {
                b"vbr\0" => gb.skip(2 + 2)?,
                b"ath\0" => ath_type = gb.u16()?,
                b"rva\0" => gb.skip(4)?,
                b"comm" => {
                    let count = usize::from(gb.u8()?);
                    gb.skip(count * 8)?;
                }
                // The cipher type: the key is given to the decoder separately.
                b"ciph" => gb.skip(2)?,
                b"loop" => gb.skip(4 + 4 + 2 + 2)?,
                _ => break,
            }
        }

        // FFmpeg computes the number of high frequency groups with unsigned arithmetic, and
        // rejects the layouts where the bands overflow the table. A layout whose base and stereo
        // bands exceed the total is never produced by an encoder, and would make the stereo
        // processing run outside of the band table.
        let low = base_band_count + stereo_band_count;
        if total_band_count < base_band_count || total_band_count < low {
            return decode_error("hca: invalid band layout");
        }

        let hfr_bands = total_band_count - low;
        let hfr_group_count =
            if bands_per_hfr_group > 0 { hfr_bands.div_ceil(bands_per_hfr_group) } else { 0 };

        if low + hfr_group_count > BANDS {
            return decode_error("hca: invalid band layout");
        }

        Ok(Header {
            channels,
            sample_rate,
            block_size,
            track_count: usize::from(track_count.max(1)),
            channel_config,
            total_band_count,
            base_band_count,
            stereo_band_count,
            bands_per_hfr_group,
            hfr_group_count,
            ath_type,
        })
    }
}
