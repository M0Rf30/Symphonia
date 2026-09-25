// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin adapters exposing the `oxideav_core::bits::{BitReader, BitWriter}`
//! method surface (`read_u32`, `read_bit`, `bit_position`; `write_bit`,
//! `write_u32`, `finish`) that the ported `oxideav-aac` 0.1.7 SBR sources
//! (see `symphonia-codec-aac/NOTICE`) call, over Symphonia's own MSB-first
//! [`BitReaderLtr`]. Keeping the method names identical meant every ported
//! file needed only a `use` change, not a body rewrite — easier to audit
//! against upstream.
//!
//! [`BitWriter`] is only used by the ported `#[cfg(test)]` modules (to
//! build synthetic bitstreams); the decode path never constructs one
//! except in [`super::element`]'s `sbr_extension()` raw-byte capture.

use symphonia_core::errors::{decode_error, Error, Result};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

/// MSB-first bit reader adapter. See the [module docs](self).
pub(crate) struct BitReader<'a> {
    inner: BitReaderLtr<'a>,
    total_bits: u64,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let inner = BitReaderLtr::new(data);
        Self { total_bits: inner.bits_left(), inner }
    }

    /// Read `n` bits (0..=32) as an unsigned integer.
    pub fn read_u32(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.inner
            .read_bits_leq32(n)
            .map_err(|_| Error::DecodeError("aac (sbr): bitreader: out of bits"))
    }

    /// Read a single bit as a bool.
    pub fn read_bit(&mut self) -> Result<bool> {
        self.inner
            .read_bool()
            .map_err(|_| Error::DecodeError("aac (sbr): bitreader: out of bits"))
    }


    /// Bits already consumed from the logical stream.
    pub fn bit_position(&self) -> u64 {
        self.total_bits - self.inner.bits_left()
    }

    /// Total remaining bits.
    pub fn bits_left(&self) -> u64 {
        self.inner.bits_left()
    }
}

/// MSB-first bit writer adapter, used only by ported unit tests to build
/// synthetic bitstreams. Not used on the decode hot path.
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    acc: u8,
    acc_bits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { bytes: Vec::new(), acc: 0, acc_bits: 0 }
    }

    pub fn write_bit(&mut self, bit: bool) {
        self.acc = (self.acc << 1) | u8::from(bit);
        self.acc_bits += 1;
        if self.acc_bits == 8 {
            self.bytes.push(self.acc);
            self.acc = 0;
            self.acc_bits = 0;
        }
    }

    pub fn write_u32(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    /// Pad the final byte with zero bits and consume the writer.
    pub fn finish(mut self) -> Vec<u8> {
        while self.acc_bits != 0 {
            self.write_bit(false);
        }
        self.bytes
    }
}

/// Map a low-level bit-reader I/O failure (only ever "out of bits") onto
/// a decode error with a static, greppable message.
#[allow(dead_code)]
pub(crate) fn bits_exhausted<T>(what: &'static str) -> Result<T> {
    decode_error(what)
}
