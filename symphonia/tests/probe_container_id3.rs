// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that ID3v2 tags stored inside container chunks/boxes are read through the default probe.

#![cfg(feature = "id3v2")]

mod common;

use common::{id3v2_tag, probe, titles};
use symphonia::core::formats::FormatReader;

#[cfg(feature = "dsd")]
fn dff(id3_id: &[u8; 4], id3_before_data: bool) -> Vec<u8> {
    let mut chunks = b"DSD ".to_vec();
    // FVER
    chunks.extend_from_slice(b"FVER");
    chunks.extend_from_slice(&4u64.to_be_bytes());
    chunks.extend_from_slice(&[1, 5, 0, 0]);
    // PROP
    let mut prop = b"SND ".to_vec();
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&10u64.to_be_bytes());
    prop.extend_from_slice(&2u16.to_be_bytes());
    prop.extend_from_slice(b"SLFTSRGT");
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&5u64.to_be_bytes());
    prop.extend_from_slice(b"DSD \x00");
    prop.push(0);
    chunks.extend_from_slice(b"PROP");
    chunks.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    chunks.extend_from_slice(&prop);

    let mut id3 = id3_id.to_vec();
    let tag = id3v2_tag(3, "DFF title");
    id3.extend_from_slice(&(tag.len() as u64).to_be_bytes());
    id3.extend_from_slice(&tag);
    if tag.len() % 2 == 1 {
        id3.push(0);
    }

    if id3_before_data {
        chunks.extend_from_slice(&id3);
    }
    chunks.extend_from_slice(b"DSD ");
    chunks.extend_from_slice(&4096u64.to_be_bytes());
    chunks.extend_from_slice(&[0x69; 4096]);
    if !id3_before_data {
        chunks.extend_from_slice(&id3);
    }

    let mut out = b"FRM8".to_vec();
    out.extend_from_slice(&(chunks.len() as u64).to_be_bytes());
    out.extend_from_slice(&chunks);
    out
}

#[cfg(feature = "dsd")]
#[test]
fn dff_id3_chunk_after_data() {
    let mut format = probe(dff(b"ID3 ", false));
    assert_eq!(format.format_info().short_name, "dff");
    assert_eq!(titles(format.as_mut()), ["DFF title"]);
}

#[cfg(feature = "dsd")]
#[test]
fn dff_lowercase_id3_chunk_after_data() {
    let mut format = probe(dff(b"id3 ", false));
    assert_eq!(titles(format.as_mut()), ["DFF title"]);
}

#[cfg(feature = "dsd")]
#[test]
fn dff_id3_chunk_before_data() {
    let mut format = probe(dff(b"ID3 ", true));
    assert_eq!(titles(format.as_mut()), ["DFF title"]);
    assert!(format.next_packet().unwrap().is_some());
}

#[cfg(feature = "caf")]
fn caf(id3_id: &[u8; 4], id3_before_data: bool) -> Vec<u8> {
    let mut out = b"caff".to_vec();
    out.extend_from_slice(&[0, 1, 0, 0]);
    out.extend_from_slice(b"desc");
    out.extend_from_slice(&32i64.to_be_bytes());
    out.extend_from_slice(&8000f64.to_be_bytes());
    out.extend_from_slice(b"lpcm");
    out.extend_from_slice(&0u32.to_be_bytes()); // big-endian signed integer
    out.extend_from_slice(&2u32.to_be_bytes()); // bytes per packet
    out.extend_from_slice(&1u32.to_be_bytes()); // frames per packet
    out.extend_from_slice(&1u32.to_be_bytes()); // channels
    out.extend_from_slice(&16u32.to_be_bytes()); // bits per channel

    let tag = id3v2_tag(3, "CAF title");
    let mut id3 = id3_id.to_vec();
    id3.extend_from_slice(&(tag.len() as i64).to_be_bytes());
    id3.extend_from_slice(&tag);

    if id3_before_data {
        out.extend_from_slice(&id3);
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(4i64 + 2000).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&[0u8; 2000]);
    if !id3_before_data {
        // The tag ends the file: the probe's appended-tag search must leave it to the container.
        out.extend_from_slice(&id3);
    }
    out
}

#[cfg(feature = "caf")]
#[test]
fn caf_id3_chunk_after_data() {
    let mut format = probe(caf(b"ID3 ", false));
    assert_eq!(format.format_info().short_name, "caf");
    assert_eq!(titles(format.as_mut()), ["CAF title"]);
    assert!(format.next_packet().unwrap().is_some());
}

#[cfg(feature = "caf")]
#[test]
fn caf_lowercase_id3_chunk_before_data() {
    let mut format = probe(caf(b"id3 ", true));
    assert_eq!(titles(format.as_mut()), ["CAF title"]);
}

#[cfg(feature = "aiff")]
fn aiff(id3_id: &[u8; 4], trailing_junk: bool) -> Vec<u8> {
    let mut chunks = b"AIFF".to_vec();
    chunks.extend_from_slice(b"COMM");
    chunks.extend_from_slice(&18u32.to_be_bytes());
    chunks.extend_from_slice(&1u16.to_be_bytes());
    chunks.extend_from_slice(&500u32.to_be_bytes());
    chunks.extend_from_slice(&16u16.to_be_bytes());
    // 8000 Hz as an 80-bit extended float.
    chunks.extend_from_slice(&[0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0]);
    chunks.extend_from_slice(b"SSND");
    chunks.extend_from_slice(&(8u32 + 1000).to_be_bytes());
    chunks.extend_from_slice(&[0u8; 8]);
    chunks.extend_from_slice(&[0u8; 1000]);

    let mut tag = id3v2_tag(3, "AIFF title");
    let tag_len = tag.len();
    // Bytes in the chunk after the tag that must not be parsed as part of it.
    if trailing_junk {
        tag.extend_from_slice(&[0xff; 6]);
    }
    if tag.len() % 2 == 1 {
        tag.push(0);
    }
    assert!(tag.len() >= tag_len);
    chunks.extend_from_slice(id3_id);
    chunks.extend_from_slice(&(tag.len() as u32).to_be_bytes());
    chunks.extend_from_slice(&tag);

    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    out.extend_from_slice(&chunks);
    out
}

#[cfg(feature = "aiff")]
#[test]
fn aiff_uppercase_id3_chunk() {
    let mut format = probe(aiff(b"ID3 ", false));
    assert_eq!(format.format_info().short_name, "aiff");
    assert_eq!(titles(format.as_mut()), ["AIFF title"]);
}

#[cfg(feature = "aiff")]
#[test]
fn aiff_lowercase_id3_chunk() {
    let mut format = probe(aiff(b"id3 ", false));
    assert_eq!(titles(format.as_mut()), ["AIFF title"]);
}

#[cfg(feature = "aiff")]
#[test]
fn aiff_id3_chunk_with_trailing_bytes_is_scoped() {
    let mut format = probe(aiff(b"ID3 ", true));
    assert_eq!(titles(format.as_mut()), ["AIFF title"]);
}

#[cfg(feature = "musepack")]
const MPC_SV8: &[u8] = include_bytes!("../../symphonia-bundle-musepack/tests/fixture_mono_q2.mpc");

#[cfg(feature = "musepack")]
fn count_packets(format: &mut dyn FormatReader) -> usize {
    let mut n = 0;
    while let Ok(Some(_)) = format.next_packet() {
        n += 1;
    }
    n
}

#[cfg(feature = "musepack")]
#[test]
fn musepack_sv8_with_leading_id3v2_probes_and_plays() {
    let mut plain = probe(MPC_SV8.to_vec());
    let expected = count_packets(plain.as_mut());
    assert!(expected > 0);

    let stream = [id3v2_tag(3, "Leading"), MPC_SV8.to_vec()].concat();
    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "musepack");
    assert_eq!(format.tracks().len(), 1);
    assert_eq!(titles(format.as_mut()), ["Leading"]);
    assert_eq!(count_packets(format.as_mut()), expected);
}

#[cfg(feature = "musepack")]
#[test]
fn musepack_sv8_with_leading_id3v2_seeks_like_without() {
    use symphonia::core::formats::{SeekMode, SeekTo};
    use symphonia::core::units::Timestamp;

    let seek = |data: Vec<u8>| {
        let mut format = probe(data);
        let seeked = format
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(44100), track_id: 0 })
            .expect("seek");
        (seeked.actual_ts, count_packets(format.as_mut()))
    };

    let expected = seek(MPC_SV8.to_vec());
    let stream = [id3v2_tag(4, "Leading v2.4"), MPC_SV8.to_vec()].concat();
    assert_eq!(seek(stream), expected);
}
