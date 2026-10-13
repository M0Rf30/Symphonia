#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the tag-only containers: ADTS AAC, AC-3, TAK, OptimFROG (APEv2) and True Audio (ID3).
//! The files are built here from a few identifying bytes and junk standing in for audio.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use symphonia::core::meta::{RawTag, RawValue, StandardTag, StandardVisualKey, Tag, Visual};
use symphonia_tagging::{
    ApePolicy, Container, Error, Id3v1Policy, ReadTags, WriteOptions, read_tags, sniff, write_file,
};

const APE_MAGIC: &[u8] = b"APETAGEX";

fn make_file(name: &str, bytes: &[u8]) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-apec-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

fn junk(len: usize, seed: u32) -> impl Iterator<Item = u8> {
    // Avoid 0xFF and 0x0B so the junk never forms a sync word by accident.
    (0..len as u32).map(move |i| ((i * 7 + seed) % 200 + 1) as u8 + 16)
}

/// `frames` ADTS frames of `frame_len` bytes: valid 7-byte headers (MPEG-4 AAC LC, 44.1 kHz,
/// stereo, no CRC) followed by junk.
fn adts(frames: usize, frame_len: usize) -> Vec<u8> {
    let mut v = Vec::new();

    for f in 0..frames {
        v.extend_from_slice(&[
            0xFF,
            0xF1,
            0x50,
            0x80 | ((frame_len >> 11) & 0x03) as u8,
            ((frame_len >> 3) & 0xFF) as u8,
            (((frame_len & 0x07) << 5) | 0x1F) as u8,
            0xFC,
        ]);
        v.extend(junk(frame_len - 7, f as u32));
    }

    v
}

/// `frames` AC-3 sync frames of `frame_len` bytes.
fn ac3(frames: usize, frame_len: usize) -> Vec<u8> {
    let mut v = Vec::new();

    for f in 0..frames {
        v.extend_from_slice(&[0x0B, 0x77]);
        v.extend(junk(frame_len - 2, f as u32));
    }

    v
}

fn headed(magic: &[u8]) -> Vec<u8> {
    let mut v = magic.to_vec();
    v.extend(junk(500, 3));
    v
}

fn id3v1_tag() -> Vec<u8> {
    let mut t = b"TAG".to_vec();
    t.resize(128, b' ');
    t
}

fn id3v2_tag() -> Vec<u8> {
    // An empty ID3v2.4 tag with 20 bytes of padding.
    let mut t = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec();
    t.extend_from_slice(&[0; 20]);
    t
}

fn std_tags(tags: &[Tag]) -> Vec<StandardTag> {
    let mut out: Vec<_> = tags.iter().filter_map(|t| t.std.clone()).collect();
    out.sort_by_key(|t| format!("{t:?}"));
    out
}

fn new_tags(title: &str) -> Vec<Tag> {
    vec![
        Tag::new_std(RawTag::new("Title", title), StandardTag::TrackTitle(s(title))),
        Tag::new_std(RawTag::new("Artist", "First"), StandardTag::Artist(s("First"))),
        Tag::new_std(RawTag::new("Artist", "Second"), StandardTag::Artist(s("Second"))),
        Tag::new_std(RawTag::new("Track", "3/12"), StandardTag::TrackNumber(3)),
        Tag::new_std(RawTag::new("Track", "3/12"), StandardTag::TrackTotal(12)),
        Tag::new_std(RawTag::new("x", "Rock"), StandardTag::Genre(s("Rock"))),
        Tag::new(RawTag::new("My Custom Key", "custom value")),
    ]
}

fn png() -> Box<[u8]> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(b"fake-idat");
    v.into_boxed_slice()
}

fn front_cover() -> Visual {
    Visual {
        media_type: None,
        dimensions: None,
        color_mode: None,
        usage: Some(StandardVisualKey::FrontCover),
        tags: vec![],
        data: png(),
    }
}

fn assert_read_back(read: &ReadTags, title: &str) {
    let std = std_tags(&read.tags);

    for expected in [
        StandardTag::TrackTitle(s(title)),
        StandardTag::Artist(s("First")),
        StandardTag::Artist(s("Second")),
        StandardTag::TrackNumber(3),
        StandardTag::TrackTotal(12),
        StandardTag::Genre(s("Rock")),
    ] {
        assert!(std.contains(&expected), "missing {expected:?} in {std:?}");
    }

    // APEv2 keeps the key. ID3v2 reports a `TXXX` frame with the key as its description.
    assert!(read.tags.iter().any(|t| {
        let key_matches = t.raw.key == "My Custom Key"
            || t.raw.sub_fields.as_ref().is_some_and(|f| {
                f.iter().any(|f| f.field == "DESCRIPTION" && f.value.to_string() == "My Custom Key")
            });
        t.std.is_none()
            && key_matches
            && matches!(&t.raw.value, RawValue::String(v) if **v == "custom value")
    }));

    assert_eq!(read.visuals.len(), 1);
    assert_eq!(read.visuals[0].usage, Some(StandardVisualKey::FrontCover));
    assert_eq!(read.visuals[0].media_type.as_deref(), Some("image/png"));
    assert_eq!(read.visuals[0].data, png());
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

/// The containers whose tags are an APEv2 tag at the end of the file.
fn ape_subjects() -> Vec<(&'static str, Container, Vec<u8>)> {
    vec![
        ("a.aac", Container::Aac, adts(6, 180)),
        ("a.ac3", Container::Ac3, ac3(5, 128)),
        ("a.tak", Container::Tak, headed(b"tBaK")),
        ("a.ofr", Container::OptimFrog, headed(b"OFR ")),
    ]
}

#[test]
fn sniff_recognises_the_new_containers() {
    for (name, container, audio) in ape_subjects() {
        let path = make_file(name, &audio);
        assert_eq!(sniff(&path).unwrap(), container, "{name}");

        // A leading ID3v2 tag, or several, is skipped.
        let mut with_id3 = id3v2_tag();
        with_id3.extend_from_slice(&id3v2_tag());
        with_id3.extend_from_slice(&audio);
        let path = make_file(name, &with_id3);
        assert_eq!(sniff(&path).unwrap(), container, "{name} behind ID3v2 tags");
        cleanup(&path);
    }

    let path = make_file("a.tta", &headed(b"TTA1"));
    assert_eq!(sniff(&path).unwrap(), Container::Tta);
    cleanup(&path);
}

#[test]
fn sniff_ignores_the_footer_flag_of_id3v2_3_tags() {
    // In ID3v2.3 the 0x10 flag bit is undefined: it does not announce a footer.
    let mut bytes = b"ID3\x03\x00\x10\x00\x00\x00\x14".to_vec();
    bytes.extend_from_slice(&[0; 20]);
    bytes.extend(headed(b"wvpk"));

    let path = make_file("a.wv", &bytes);
    assert_eq!(sniff(&path).unwrap(), Container::WavPack);
    cleanup(&path);
}

#[test]
fn mpeg_audio_is_still_mpeg_audio() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

    for name in ["mpeg-plain.mp3", "mpeg-v23.mp3", "mpeg-v24.mp3", "mpeg-ape.mp3"] {
        assert_eq!(sniff(&dir.join(name)).unwrap(), Container::MpegAudio, "{name}");
    }

    // MPEG-2 layer III, MPEG-1 layer III and MPEG-2.5 headers.
    for header in [[0xFF, 0xF3], [0xFF, 0xFB], [0xFF, 0xE3]] {
        let mut bytes = header.to_vec();
        bytes.extend(junk(300, 1));
        let path = make_file("a.mp3", &bytes);
        assert_eq!(sniff(&path).unwrap(), Container::MpegAudio, "{header:02x?}");
        cleanup(&path);
    }
}

#[test]
fn an_adts_header_without_a_following_frame_is_not_aac() {
    // The first header looks like ADTS (frame length 180), but no header follows the frame.
    let mut bytes = adts(1, 180);
    bytes.extend(junk(300, 5));
    let path = make_file("a.mp3", &bytes);
    assert_ne!(sniff(&path).unwrap(), Container::Aac);
    cleanup(&path);

    // A stream that is a single frame is ADTS.
    let path = make_file("a.aac", &adts(1, 180));
    assert_eq!(sniff(&path).unwrap(), Container::Aac);
    cleanup(&path);
}

#[test]
fn write_then_read_round_trips_the_tags_and_a_cover() {
    for (name, container, audio) in ape_subjects() {
        for tail in [Vec::new(), id3v1_tag()] {
            for prefix in [Vec::new(), id3v2_tag()] {
                let mut original = prefix.clone();
                original.extend_from_slice(&audio);
                original.extend_from_slice(&tail);
                let path = make_file(name, &original);

                // Nothing to read from a file without tags.
                let empty = read_tags(&path).unwrap();
                assert!(empty.tags.is_empty() && empty.visuals.is_empty(), "{name}");

                write_file(
                    &path,
                    &new_tags("New Title"),
                    &[front_cover()],
                    &WriteOptions::default(),
                )
                .unwrap();
                assert_eq!(sniff(&path).unwrap(), container);

                let written = fs::read(&path).unwrap();
                let audio_end = prefix.len() + audio.len();
                assert_eq!(&written[..audio_end], &original[..audio_end], "{name}: audio changed");
                assert!(written.ends_with(&tail), "{name}: the ID3v1 tag moved or changed");
                assert_eq!(&written[audio_end..audio_end + 8], APE_MAGIC, "{name}");

                assert_read_back(&read_tags(&path).unwrap(), "New Title");
                cleanup(&path);
            }
        }
    }
}

#[test]
fn rewriting_replaces_the_ape_tag() {
    for (name, _, audio) in ape_subjects() {
        for tail in [Vec::new(), id3v1_tag()] {
            let mut original = audio.clone();
            original.extend_from_slice(&tail);
            let path = make_file(name, &original);

            write_file(&path, &new_tags("First Title"), &[front_cover()], &WriteOptions::default())
                .unwrap();
            write_file(
                &path,
                &new_tags("Second Title"),
                &[front_cover()],
                &WriteOptions::default(),
            )
            .unwrap();

            let bytes = fs::read(&path).unwrap();
            assert_eq!(count(&bytes, APE_MAGIC), 2, "{name}: one header and one footer");
            assert!(bytes.ends_with(&tail));

            let read = read_tags(&path).unwrap();
            assert_read_back(&read, "Second Title");
            assert!(!std_tags(&read.tags).contains(&StandardTag::TrackTitle(s("First Title"))));

            // Removing the tags restores the stream and its ID3v1 tag.
            write_file(&path, &[], &[], &WriteOptions::default()).unwrap();
            assert_eq!(fs::read(&path).unwrap(), original, "{name}");
            cleanup(&path);
        }
    }
}

#[test]
fn ape_policies_of_aac_and_ac3() {
    let subjects =
        [("a.aac", Container::Aac, adts(6, 180)), ("a.ac3", Container::Ac3, ac3(5, 128))];

    for (name, _, audio) in subjects {
        let options = |policy| {
            if name.ends_with("aac") {
                WriteOptions { aac_ape: policy, ..Default::default() }
            }
            else {
                WriteOptions { ac3_ape: policy, ..Default::default() }
            }
        };

        let mut tagged = audio.clone();
        tagged.extend_from_slice(&id3v1_tag());

        // Start from a file that carries an APE tag.
        let path = make_file(name, &tagged);
        write_file(&path, &new_tags("Old"), &[], &WriteOptions::default()).unwrap();
        let with_ape = fs::read(&path).unwrap();
        assert_ne!(with_ape, tagged);
        cleanup(&path);

        // Keep: neither tags nor policy-less rewrites change the file, whatever the tags are.
        for (start, tags) in
            [(&tagged, new_tags("New")), (&with_ape, new_tags("New")), (&with_ape, vec![])]
        {
            let path = make_file(name, start);
            write_file(&path, &tags, &[front_cover()], &options(ApePolicy::Keep)).unwrap();
            assert_eq!(&fs::read(&path).unwrap(), start, "{name}: Keep must not touch the file");
            cleanup(&path);
        }

        // Remove: an existing tag is dropped, whatever the tags are, and the ID3v1 tag stays.
        let path = make_file(name, &with_ape);
        write_file(&path, &new_tags("New"), &[], &options(ApePolicy::Remove)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), tagged, "{name}");
        cleanup(&path);

        // Remove on a file without a tag is a no-op.
        let path = make_file(name, &tagged);
        write_file(&path, &new_tags("New"), &[], &options(ApePolicy::Remove)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), tagged, "{name}");
        cleanup(&path);

        // Write: the tag is replaced.
        let path = make_file(name, &with_ape);
        write_file(&path, &new_tags("New"), &[front_cover()], &options(ApePolicy::Write)).unwrap();
        let read = read_tags(&path).unwrap();
        assert_read_back(&read, "New");
        assert!(!std_tags(&read.tags).contains(&StandardTag::TrackTitle(s("Old"))));
        assert_eq!(count(&fs::read(&path).unwrap(), APE_MAGIC), 2);
        cleanup(&path);
    }
}

#[test]
fn id3v1_policy_does_not_apply_to_ape_containers() {
    for (name, _, audio) in ape_subjects() {
        let path = make_file(name, &audio);
        let opts = WriteOptions { id3v1: Id3v1Policy::Write, ..Default::default() };
        write_file(&path, &new_tags("T"), &[], &opts).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes[audio.len()..].starts_with(APE_MAGIC), "{name}");
        assert_ne!(&bytes[bytes.len() - 128..bytes.len() - 125], b"TAG", "{name}");
        cleanup(&path);
    }
}

#[test]
fn true_audio_round_trips_id3v2_and_id3v1() {
    let audio = headed(b"TTA1");
    let path = make_file("a.tta", &audio);
    assert_eq!(sniff(&path).unwrap(), Container::Tta);

    let tags = new_tags("TTA Title");
    let opts = WriteOptions { id3v1: Id3v1Policy::Write, padding: 0, ..Default::default() };
    write_file(&path, &tags, &[front_cover()], &opts).unwrap();

    let written = fs::read(&path).unwrap();
    assert_eq!(&written[..3], b"ID3");
    assert_eq!(&written[written.len() - 128..written.len() - 125], b"TAG");
    assert!(contains(&written, &audio), "audio bytes changed");
    assert_eq!(sniff(&path).unwrap(), Container::Tta);

    let read = read_tags(&path).unwrap();
    assert_read_back(&read, "TTA Title");

    // The ID3v1 tag has the lowest priority: it is last, and the ID3v2 title comes first.
    let titles: Vec<_> = read
        .tags
        .iter()
        .filter(|t| matches!(t.std, Some(StandardTag::TrackTitle(_))))
        .map(|t| t.raw.key.clone())
        .collect();
    assert_eq!(titles.len(), 2, "{titles:?}");
    assert_eq!(titles[1], "TITLE", "ID3v1 title last: {titles:?}");
    assert_ne!(titles[0], "TITLE");
    cleanup(&path);
}

#[test]
fn read_tags_reads_the_ape_tag_of_a_tta_file() {
    // TTA is written with ID3, but an APEv2 tag appended by another tool is read as well.
    let audio = headed(b"TTA1");
    let path = make_file("a.tta", &audio);

    let ape_path = make_file("a.tak", &headed(b"tBaK"));
    write_file(&ape_path, &new_tags("From APE"), &[front_cover()], &WriteOptions::default())
        .unwrap();
    let ape = fs::read(&ape_path).unwrap()[audio.len()..].to_vec();

    let mut bytes = audio.clone();
    bytes.extend_from_slice(&ape);
    fs::write(&path, &bytes).unwrap();

    assert_read_back(&read_tags(&path).unwrap(), "From APE");
    cleanup(&path);
    cleanup(&ape_path);
}

#[test]
fn read_tags_rejects_containers_the_probe_reads() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mpeg-plain.mp3");
    assert!(matches!(read_tags(&fixture), Err(Error::Unsupported(_))));

    let path = make_file("a.bin", &junk(100, 0).collect::<Vec<_>>());
    assert!(matches!(read_tags(&path), Err(Error::Unsupported(_))));
    cleanup(&path);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
