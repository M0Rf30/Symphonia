// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A bit reader with the semantics of FFmpeg's `GetBitContext` that the WMA decoder relies on.

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::vlc::{Codebook, CodebookEntry};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

/// The number of zero bytes that must follow the data of a [`Bits`] buffer.
///
/// FFmpeg guarantees `AV_INPUT_BUFFER_PADDING_SIZE` (64) readable bytes after every packet, and
/// the WMA decoder may read a few bits beyond the end of a frame whose end is not coded.
pub const PADDING: usize = 8;

/// A most-significant-bit-first bit reader over a buffer that is followed by [`PADDING`] zero
/// bytes.
///
/// The reader knows the number of valid bits (`limit`), which may be fewer than the bits of the
/// buffer, but only the padded buffer bounds the reads: reading past the limit yields the padding
/// zeros, as in FFmpeg.
pub struct Bits<'a> {
    reader: BitReaderLtr<'a>,
    limit: u64,
    pos: u64,
}

impl<'a> Bits<'a> {
    /// `buf` is the data followed by the padding. `limit` is the number of valid bits.
    pub fn new(buf: &'a [u8], limit: u64) -> Self {
        Bits { reader: BitReaderLtr::new(buf), limit, pos: 0 }
    }

    /// Reads `n <= 32` bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32> {
        match self.reader.read_bits_leq32(n) {
            Ok(v) => {
                self.pos += u64::from(n);
                Ok(v)
            }
            Err(_) => decode_error("wma: bitstream overrun"),
        }
    }

    /// Reads one bit.
    #[inline]
    pub fn bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, mut n: u32) -> Result<()> {
        while n > 0 {
            let step = n.min(32);
            self.read(step)?;
            n -= step;
        }
        Ok(())
    }

    /// Skips to the next byte boundary (`align_get_bits`).
    pub fn align(&mut self) -> Result<()> {
        self.skip((self.pos.wrapping_neg() & 7) as u32)
    }

    /// The number of bits that were read (`get_bits_count`).
    pub fn count(&self) -> u64 {
        self.pos
    }

    /// The number of valid bits that are left (`get_bits_left`). Negative after reading past the
    /// valid bits.
    pub fn left(&self) -> i64 {
        self.limit as i64 - self.pos as i64
    }

    /// Reads a value with a codebook.
    #[inline]
    pub fn codebook<E: CodebookEntry>(&mut self, codebook: &Codebook<E>) -> Result<E::ValueType> {
        match self.reader.read_codebook(codebook) {
            Ok((value, len)) => {
                self.pos += u64::from(len);
                Ok(value)
            }
            Err(_) => decode_error("wma: invalid codeword"),
        }
    }
}
