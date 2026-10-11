// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests of frame content mapping to standard tags.

use std::io::Cursor;

use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::{MetadataOptions, MetadataReader, StandardTag, Tag};

use super::Id3v2Reader;

fn syncsafe(n: usize) -> [u8; 4] {
    [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
}

/// Builds an ID3v2.3 frame.
fn frame_v3(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut f = id.to_vec();
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(body);
    f
}

/// Builds an ID3v2.2 frame.
fn frame_v2(id: &[u8; 3], body: &[u8]) -> Vec<u8> {
    let mut f = id.to_vec();
    f.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    f.extend_from_slice(body);
    f
}

/// Builds an ISO-8859-1 text frame body from the null-separated strings.
fn text_body(strings: &[&str]) -> Vec<u8> {
    let mut b = vec![0u8];
    b.extend_from_slice(strings.join("\0").as_bytes());
    b
}

fn tag(major: u8, frames: Vec<Vec<u8>>) -> Vec<u8> {
    let frames = frames.concat();
    let mut t = vec![b'I', b'D', b'3', major, 0, 0];
    t.extend_from_slice(&syncsafe(frames.len()));
    t.extend_from_slice(&frames);
    t
}

fn read_tags(tag: Vec<u8>) -> Vec<Tag> {
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(tag)), MediaSourceStreamOptions::default());
    let mut reader = Id3v2Reader::try_new(mss, MetadataOptions::default()).unwrap();
    reader.read_all().unwrap().revision.media.tags
}

fn std_tags(tags: &[Tag]) -> Vec<StandardTag> {
    tags.iter().filter_map(|t| t.std.clone()).collect()
}

fn has_recording_date(tags: &[StandardTag], date: &str) -> bool {
    tags.iter().any(|t| matches!(t, StandardTag::RecordingDate(d) if d.as_str() == date))
}

#[test]
fn verify_tyer_tdat_time_combine_into_recording_date() {
    let tags = std_tags(&read_tags(tag(
        3,
        vec![frame_v3(b"TYER", &text_body(&["2004"])), frame_v3(b"TDAT", &text_body(&["1503"]))],
    )));

    assert!(has_recording_date(&tags, "2004-03-15"));
    assert!(tags.iter().any(|t| matches!(t, StandardTag::RecordingYear(2004))));
    // The raw DDMM must never be exposed as the recording date.
    assert!(!has_recording_date(&tags, "1503"));

    let tags = std_tags(&read_tags(tag(
        3,
        vec![
            frame_v3(b"TIME", &text_body(&["1430"])),
            frame_v3(b"TDAT", &text_body(&["1503"])),
            frame_v3(b"TYER", &text_body(&["2004"])),
        ],
    )));
    assert!(has_recording_date(&tags, "2004-03-15T14:30"));
}

#[test]
fn verify_tyer_alone_and_invalid_tdat() {
    let tags = std_tags(&read_tags(tag(3, vec![frame_v3(b"TYER", &text_body(&["2004"]))])));
    assert!(has_recording_date(&tags, "2004"));

    // An invalid DDMM falls back to the year.
    let tags = std_tags(&read_tags(tag(
        3,
        vec![frame_v3(b"TYER", &text_body(&["2004"])), frame_v3(b"TDAT", &text_body(&["3113"]))],
    )));
    assert!(has_recording_date(&tags, "2004"));
}

#[test]
fn verify_tdat_alone_has_no_recording_date() {
    let tags = std_tags(&read_tags(tag(3, vec![frame_v3(b"TDAT", &text_body(&["1503"]))])));
    assert!(!tags.iter().any(|t| matches!(t, StandardTag::RecordingDate(_))));

    let tags = std_tags(&read_tags(tag(3, vec![frame_v3(b"TRDA", &text_body(&["March 15"]))])));
    assert!(!tags.iter().any(|t| matches!(t, StandardTag::RecordingDate(_))));
}

#[test]
fn verify_v22_date_frames_combine() {
    let tags = std_tags(&read_tags(tag(
        2,
        vec![
            frame_v2(b"TYE", &text_body(&["2004"])),
            frame_v2(b"TDA", &text_body(&["1503"])),
            frame_v2(b"TIM", &text_body(&["0905"])),
        ],
    )));
    assert!(has_recording_date(&tags, "2004-03-15T09:05"));
    assert!(!has_recording_date(&tags, "1503"));
}

#[test]
fn verify_trck_with_whitespace() {
    let tags = std_tags(&read_tags(tag(3, vec![frame_v3(b"TRCK", &text_body(&["3 / 12"]))])));
    assert!(tags.iter().any(|t| matches!(t, StandardTag::TrackNumber(3))));
    assert!(tags.iter().any(|t| matches!(t, StandardTag::TrackTotal(12))));
}

#[test]
fn verify_tmcl_maps_to_performer() {
    let tags = std_tags(&read_tags(tag(
        4,
        vec![frame_v3(b"TMCL", &text_body(&["piano", "Glenn Gould", "violin", "Yehudi Menuhin"]))],
    )));
    assert!(
        tags.iter().any(|t| matches!(t, StandardTag::Performer(p) if p.as_str() == "Glenn Gould"))
    );
    assert!(
        tags.iter()
            .any(|t| matches!(t, StandardTag::Performer(p) if p.as_str() == "Yehudi Menuhin"))
    );
}

#[test]
fn verify_ipls_roles() {
    let tags = std_tags(&read_tags(tag(
        3,
        vec![frame_v3(b"IPLS", &text_body(&["Composer", "J. S. Bach", "unknown role", "X"]))],
    )));
    assert!(
        tags.iter().any(|t| matches!(t, StandardTag::Composer(p) if p.as_str() == "J. S. Bach"))
    );
}

fn txxx(desc: &str, value: &str) -> Vec<u8> {
    frame_v3(b"TXXX", &text_body(&[desc, value]))
}

#[test]
fn verify_txxx_descriptions() {
    let tags = std_tags(&read_tags(tag(
        3,
        vec![
            txxx("LABEL", "Deutsche Grammophon"),
            txxx("originalyear", "1955"),
            txxx("MusicBrainz Release Track Id", "abc"),
            txxx("ALBUMSORT", "Goldberg"),
        ],
    )));
    assert!(
        tags.iter()
            .any(|t| matches!(t, StandardTag::Label(l) if l.as_str() == "Deutsche Grammophon"))
    );
    assert!(tags.iter().any(|t| matches!(t, StandardTag::OriginalReleaseYear(1955))));
    assert!(tags.iter().any(|t| matches!(t, StandardTag::MusicBrainzReleaseTrackId(_))));
    assert!(tags.iter().any(|t| matches!(t, StandardTag::SortAlbum(_))));
}

fn rva2(id: &str, adjustment: i16, peak_bits: u8, peak: &[u8]) -> Vec<u8> {
    let mut b = id.as_bytes().to_vec();
    b.push(0);
    b.push(1);
    b.extend_from_slice(&adjustment.to_be_bytes());
    b.push(peak_bits);
    b.extend_from_slice(peak);
    frame_v3(b"RVA2", &b)
}

#[test]
fn verify_rva2_replaygain() {
    // -6.5 dB = -3328 / 512, peak 0.5.
    let tags = std_tags(&read_tags(tag(
        4,
        vec![rva2("track", -3328, 16, &[0x40, 0x00]), rva2("album", 512, 0, &[])],
    )));
    assert!(
        tags.iter()
            .any(|t| matches!(t, StandardTag::ReplayGainTrackGain(g) if g.as_str() == "-6.50 dB"))
    );
    assert!(
        tags.iter()
            .any(|t| matches!(t, StandardTag::ReplayGainTrackPeak(p) if p.as_str() == "0.500000"))
    );
    assert!(
        tags.iter()
            .any(|t| matches!(t, StandardTag::ReplayGainAlbumGain(g) if g.as_str() == "1.00 dB"))
    );
    assert!(!tags.iter().any(|t| matches!(t, StandardTag::ReplayGainAlbumPeak(_))));
}
