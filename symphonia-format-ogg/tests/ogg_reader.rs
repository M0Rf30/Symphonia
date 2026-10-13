// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "writer")]
#![allow(clippy::unwrap_used)]

use std::io::Cursor;

use symphonia_core::errors::Error;
use symphonia_core::formats::FormatOptions;
use symphonia_core::formats::FormatReader;
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_ogg::OggReader;
use symphonia_format_ogg::writer::OggPacketWriter;

const FRAMES_PER_PACKET: i64 = 960;

/// Builds one Opus link with `n_packets` 20 ms audio packets, each on its own page.
fn opus_link(serial: u32, n_packets: usize) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.extend_from_slice(&[1, 2]); // version, channels
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // gain
    head.push(0); // mapping family

    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&0u32.to_le_bytes()); // vendor length
    tags.extend_from_slice(&0u32.to_le_bytes()); // comment count

    let mut w = OggPacketWriter::new(Vec::new(), serial);
    w.set_page_target(1);
    w.write_packet(&head, 0, false).unwrap();
    w.flush_page().unwrap();
    w.write_packet(&tags, 0, false).unwrap();
    w.flush_page().unwrap();

    // TOC 0xFC: CELT fullband, 20 ms, one frame (code 0).
    let mut pkt = vec![0xfc];
    pkt.extend_from_slice(&[0x55; 40]);

    for i in 0..n_packets {
        let last = i + 1 == n_packets;
        w.write_packet(&pkt, (i as i64 + 1) * FRAMES_PER_PACKET, last).unwrap();
    }
    w.finish().unwrap()
}

fn open(data: Vec<u8>) -> OggReader<'static> {
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default());
    OggReader::try_new(mss, FormatOptions::default()).unwrap()
}

/// Packets must carry `dts == pts` (audio), and pts must advance by the packet duration.
#[test]
fn packet_dts_equals_pts() {
    let mut reader = open(opus_link(0x1234, 5));

    let mut n = 0;
    while let Some(pkt) = reader.next_packet().unwrap() {
        assert_eq!(pkt.dts, pkt.pts, "packet {n}");
        assert_eq!(pkt.pts.get(), n * FRAMES_PER_PACKET, "packet {n}");
        n += 1;
    }
    assert_eq!(n, 5);
}

/// A chained stream that is cut inside the second link must report an unexpected EOF, not a
/// clean end of stream.
#[test]
fn chained_stream_truncated_in_second_link_is_an_error() {
    let link1 = opus_link(0x1111, 3);
    let link2 = opus_link(0x2222, 3);

    // Cut at several positions inside the second link, from within the head page (the new link
    // never yields a packet) to within its audio packets.
    for cut in [10usize, 60, link2.len() / 2] {
        let mut data = link1.clone();
        data.extend_from_slice(&link2[..cut]);

        let mut reader = open(data);

        let mut n = 0;
        let err = loop {
            match reader.next_packet() {
                Ok(Some(_)) => n += 1,
                Ok(None) => panic!("cut={cut}: truncated chain ended cleanly after {n} packets"),
                // The reset signals the start of the second link. Packets of the second link
                // that were fully received may follow, then the error.
                Err(Error::ResetRequired) => continue,
                Err(e) => break e,
            }
        };
        match err {
            Error::IoError(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            e => panic!("cut={cut}: unexpected error {e}"),
        }
    }
}

/// An intact chained stream still ends cleanly.
#[test]
fn chained_stream_intact_ends_cleanly() {
    let mut data = opus_link(0x1111, 3);
    data.extend_from_slice(&opus_link(0x2222, 3));

    let mut reader = open(data);
    let mut n = 0;
    loop {
        match reader.next_packet() {
            Ok(Some(_)) => n += 1,
            Ok(None) => break,
            Err(Error::ResetRequired) => (),
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(n, 6);
}
