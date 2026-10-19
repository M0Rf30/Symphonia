// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Malformed old style `.ra` files must be rejected, not buffered.

use std::io::Cursor;

use symphonia_core::formats::FormatOptions;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_real::RealReader;

/// A version 4 `.ra` header with the `Int0` interleaver.
fn v4_int0(coded_frame_size: u32, sub_packet_h: u16, frame_size: u16) -> Vec<u8> {
    let mut b = b".ra\xfd".to_vec();
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(b".ra4");
    b.extend_from_slice(&0u32.to_be_bytes()); // Data size.
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // Header size.
    b.extend_from_slice(&0u16.to_be_bytes()); // Flavor.
    b.extend_from_slice(&coded_frame_size.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // Bytes per minute.
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&sub_packet_h.to_be_bytes());
    b.extend_from_slice(&frame_size.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes()); // Sub packet size.
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&8000u16.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[4]);
    b.extend_from_slice(b"Int0");
    b.extend_from_slice(&[4]);
    b.extend_from_slice(b"28_8");
    b.extend_from_slice(&[0; 3]);
    b.extend_from_slice(&[0; 4]); // Empty text.
    b.extend_from_slice(&[0; 64]);
    b
}

fn open(data: Vec<u8>) -> symphonia_core::errors::Result<RealReader<'static>> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    RealReader::try_new(mss, FormatOptions::default())
}

#[test]
fn old_ra_huge_packet_size_is_rejected() {
    // 0x7fff_ffff * 0xffff / 2 bytes would be requested by the first packet.
    assert!(open(v4_int0(i32::MAX as u32, u16::MAX, 1)).is_err());
}

#[test]
fn old_ra_small_packet_size_is_accepted() {
    assert!(open(v4_int0(20, 2, 1)).is_ok());
}
