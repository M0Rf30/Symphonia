// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests of frame content mapping to standard tags.

use std::io::Cursor;

use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::{MetadataOptions, MetadataReader, RawValue, StandardTag, Tag};

use super::Id3v2Reader;
use super::unsync::encode_synchsafe;

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
    t.extend_from_slice(&encode_synchsafe(frames.len() as u32));
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

fn raw_tag<'a>(tags: &'a [Tag], key: &str) -> &'a Tag {
    tags.iter().find(|t| t.raw.key == key).unwrap_or_else(|| panic!("no {key} tag"))
}

fn sub_field<'a>(tag: &'a Tag, name: &str) -> &'a RawValue {
    &tag.raw
        .sub_fields
        .as_ref()
        .and_then(|s| s.iter().find(|f| f.field == name))
        .unwrap_or_else(|| panic!("no {name} sub-field"))
        .value
}

fn strings(v: &RawValue) -> Vec<&str> {
    match v {
        RawValue::StringList(l) => l.iter().map(String::as_str).collect(),
        RawValue::String(s) => vec![s.as_str()],
        other => panic!("not a string value: {other:?}"),
    }
}

#[test]
fn verify_sylt_frame() {
    let mut body = vec![0u8];
    body.extend_from_slice(b"eng");
    body.extend_from_slice(&[2, 1]);
    body.extend_from_slice(b"desc\0");
    body.extend_from_slice(b"Hel\0");
    body.extend_from_slice(&1000u32.to_be_bytes());
    body.extend_from_slice(b"lo\0");
    body.extend_from_slice(&2500u32.to_be_bytes());

    let tags = read_tags(tag(3, vec![frame_v3(b"SYLT", &body)]));
    let t = raw_tag(&tags, "SYLT");
    assert_eq!(strings(&t.raw.value), ["Hel", "lo"]);
    assert_eq!(strings(sub_field(t, "TIMESTAMPS")), ["1000", "2500"]);
    assert!(matches!(sub_field(t, "TIMESTAMP_FORMAT"), RawValue::UnsignedInt(2)));
    assert!(matches!(sub_field(t, "CONTENT_TYPE"), RawValue::UnsignedInt(1)));
    assert_eq!(strings(sub_field(t, "LANGUAGE")), ["eng"]);
    assert_eq!(strings(sub_field(t, "DESCRIPTION")), ["desc"]);
}

#[test]
fn verify_etco_frame() {
    let mut body = vec![2u8, 0x02];
    body.extend_from_slice(&0u32.to_be_bytes());
    body.push(0x03);
    body.extend_from_slice(&90_000u32.to_be_bytes());

    let tags = read_tags(tag(3, vec![frame_v3(b"ETCO", &body)]));
    let t = raw_tag(&tags, "ETCO");
    assert_eq!(strings(&t.raw.value), ["2", "3"]);
    assert_eq!(strings(sub_field(t, "TIMESTAMPS")), ["0", "90000"]);
    assert!(matches!(sub_field(t, "TIMESTAMP_FORMAT"), RawValue::UnsignedInt(2)));
}

#[test]
fn verify_sytc_frame() {
    // Tempo 120 at frame 0, then 0xff + 45 = 300 at frame 100.
    let mut body = vec![1u8, 120];
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&[0xff, 45]);
    body.extend_from_slice(&100u32.to_be_bytes());

    let tags = read_tags(tag(3, vec![frame_v3(b"SYTC", &body)]));
    let t = raw_tag(&tags, "SYTC");
    assert_eq!(strings(&t.raw.value), ["120", "300"]);
    assert_eq!(strings(sub_field(t, "TIMESTAMPS")), ["0", "100"]);
    assert!(matches!(sub_field(t, "TIMESTAMP_FORMAT"), RawValue::UnsignedInt(1)));
}

#[test]
fn verify_equ2_frame() {
    // Linear interpolation, "eq1", 1000 Hz at +1.5 dB, 20000 Hz at -2 dB.
    let mut body = vec![1u8];
    body.extend_from_slice(b"eq1\0");
    body.extend_from_slice(&2000u16.to_be_bytes());
    body.extend_from_slice(&768i16.to_be_bytes());
    body.extend_from_slice(&40000u16.to_be_bytes());
    body.extend_from_slice(&(-1024i16).to_be_bytes());

    let tags = read_tags(tag(4, vec![frame_v3(b"EQU2", &body)]));
    let t = raw_tag(&tags, "EQU2");
    assert_eq!(strings(&t.raw.value), ["eq1"]);
    assert_eq!(strings(sub_field(t, "FREQUENCIES_HZ")), ["1000", "20000"]);
    assert_eq!(strings(sub_field(t, "ADJUSTMENTS_DB")), ["1.5", "-2"]);
    assert!(matches!(sub_field(t, "INTERPOLATION_METHOD"), RawValue::UnsignedInt(1)));
}

#[test]
fn verify_geob_priv_sign_comr_frames() {
    let mut geob = vec![0u8];
    geob.extend_from_slice(b"image/png\0name.png\0a desc\0");
    geob.extend_from_slice(&[1, 2, 3]);

    let mut private = b"owner@example.com\0".to_vec();
    private.extend_from_slice(&[9, 8]);

    let mut sign = vec![7u8];
    sign.extend_from_slice(&[0xaa, 0xbb]);

    let mut comr = vec![0u8];
    comr.extend_from_slice(b"EUR1.00\0");
    comr.extend_from_slice(b"20301231");
    comr.extend_from_slice(b"http://x.example\0");
    comr.push(2);
    comr.extend_from_slice(b"Seller\0Desc\0image/png\0");
    comr.extend_from_slice(&[0x89, 0x50]);

    let tags = read_tags(tag(
        4,
        vec![
            frame_v3(b"GEOB", &geob),
            frame_v3(b"PRIV", &private),
            frame_v3(b"SIGN", &sign),
            frame_v3(b"COMR", &comr),
        ],
    ));

    let g = raw_tag(&tags, "GEOB");
    assert!(matches!(&g.raw.value, RawValue::Binary(b) if b.as_ref().as_ref() == [1, 2, 3]));
    assert_eq!(strings(sub_field(g, "MIME_TYPE")), ["image/png"]);
    assert_eq!(strings(sub_field(g, "FILE_NAME")), ["name.png"]);
    assert_eq!(strings(sub_field(g, "DESCRIPTION")), ["a desc"]);

    let p = raw_tag(&tags, "PRIV");
    assert!(matches!(&p.raw.value, RawValue::Binary(b) if b.as_ref().as_ref() == [9, 8]));
    assert_eq!(strings(sub_field(p, "OWNER")), ["owner@example.com"]);

    let s = raw_tag(&tags, "SIGN");
    assert!(matches!(&s.raw.value, RawValue::Binary(b) if b.as_ref().as_ref() == [0xaa, 0xbb]));
    assert!(matches!(sub_field(s, "GROUP_ID"), RawValue::UnsignedInt(7)));

    let c = raw_tag(&tags, "COMR");
    assert_eq!(strings(&c.raw.value), ["EUR1.00"]);
    assert_eq!(strings(sub_field(c, "VALID_UNTIL")), ["20301231"]);
    assert_eq!(strings(sub_field(c, "SELLER_NAME")), ["Seller"]);
}

#[test]
fn verify_aspi_mllt_remain_raw_binary() {
    let tags = read_tags(tag(4, vec![frame_v3(b"ASPI", &[1, 2, 3]), frame_v3(b"MLLT", &[4, 5])]));
    assert!(matches!(&raw_tag(&tags, "ASPI").raw.value, RawValue::Binary(_)));
    assert!(matches!(&raw_tag(&tags, "MLLT").raw.value, RawValue::Binary(_)));
}

#[test]
fn verify_mcdi_is_typed_as_cd_toc() {
    let toc = [0x00, 0x14, 0x01, 0x02, 0xAB, 0xCD];
    let tags = read_tags(tag(3, vec![frame_v3(b"MCDI", &toc)]));
    let t = raw_tag(&tags, "MCDI");
    assert!(matches!(&t.raw.value, RawValue::Binary(b) if b.as_ref().as_ref() == toc));
    assert!(matches!(&t.std, Some(StandardTag::CdToc(v)) if v.as_str() == "00140102ABCD"));
}

#[test]
fn verify_itunnorm_comment_is_mapped_to_replaygain() {
    let norm =
        "00001E86 00001E86 0000A2A3 0000A2A3 000006A6 000006A6 000078FA 000078FA 00000211 00000211";
    let mut body = vec![0u8];
    body.extend_from_slice(b"eng");
    body.extend_from_slice(b"iTunNORM\0");
    body.extend_from_slice(norm.as_bytes());

    let tags = read_tags(tag(3, vec![frame_v3(b"COMM", &body)]));
    let std = std_tags(&tags);

    // Not exposed as a user comment.
    assert!(!std.iter().any(|t| matches!(t, StandardTag::Comment(_))));
    // max(0x1E86 = 7814, 0xA2A3 / 2.5 = 16617) -> 16617: -10*log10(16.617) = -12.22 dB.
    assert!(
        std.iter()
            .any(|t| matches!(t, StandardTag::ReplayGainTrackGain(g) if g.as_str() == "-12.22 dB"))
    );
    // 0x78FA / 32768.
    assert!(
        std.iter()
            .any(|t| matches!(t, StandardTag::ReplayGainTrackPeak(p) if p.as_str() == "0.945129"))
    );
    // The original raw comment is retained.
    assert!(tags.iter().any(|t| t.raw.key == "COMM"));
}
