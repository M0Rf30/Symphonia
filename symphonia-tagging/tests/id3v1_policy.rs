// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the ID3v1 policy (`WriteOptions::id3v1`) and of `WriteOptions::remove_ape_from_mpeg`
//! for MPEG audio (the `mpeg-*.mp3` fixtures) and for a synthetic True Audio file.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataReader, RawTag, StandardTag, Tag};
use symphonia_metadata::id3v1::Id3v1Reader;
use symphonia_tagging::{Container, Id3v1Policy, WriteOptions, sniff, write_file};

const APE_MAGIC: &[u8] = b"APETAGEX";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn temp_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-id3v1-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `bytes` to a new file in a private temporary directory.
fn make_file(name: &str, bytes: &[u8]) -> PathBuf {
    let path = temp_dir().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

fn std_tag(std: StandardTag) -> Tag {
    Tag::new_std(RawTag::new("", ""), std)
}

fn new_tags() -> Vec<Tag> {
    vec![
        std_tag(StandardTag::TrackTitle(s("New Title"))),
        std_tag(StandardTag::Artist(s("New Artist"))),
        std_tag(StandardTag::Album(s("New Album"))),
        std_tag(StandardTag::TrackNumber(7)),
        std_tag(StandardTag::Genre(s("Rock"))),
    ]
}

fn opts(id3v1: Id3v1Policy) -> WriteOptions {
    WriteOptions { id3v1, padding: 0, ..Default::default() }
}

/// The MPEG stream of the fixtures: a file without any tag.
fn mpeg_audio() -> Vec<u8> {
    fs::read(fixture("mpeg-plain.mp3")).unwrap()
}

/// A synthetic True Audio file: the identifying header followed by junk standing in for audio.
fn tta_audio() -> Vec<u8> {
    let mut v = b"TTA1".to_vec();
    v.extend((0..400u32).map(|i| (i * 13 % 251) as u8));
    v
}

fn old_id3v1() -> Vec<u8> {
    let mut t = vec![0u8; 128];
    t[..3].copy_from_slice(b"TAG");
    t[3..6].copy_from_slice(b"Old");
    t[127] = 255;
    t
}

fn enhanced_id3v1() -> Vec<u8> {
    let mut t = vec![0u8; 227];
    t[..4].copy_from_slice(b"TAG+");
    t[4..16].copy_from_slice(b"Enhanced Old");
    t
}

fn has_id3v1(bytes: &[u8]) -> bool {
    bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG"
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The length of the ID3v2 tag at the start of `bytes`, if there is one.
fn leading_id3v2_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return None;
    }

    let size = bytes[6..10].iter().fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f));
    Some(10 + size + if bytes[3] == 4 && bytes[5] & 0x10 != 0 { 10 } else { 0 })
}

/// `bytes` without the leading ID3v2 tag(s) and without the trailing ID3v1 tag(s).
fn core(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while let Some(len) = leading_id3v2_len(&bytes[start..]) {
        start += len;
    }

    let mut end = bytes.len();
    if has_id3v1(bytes) {
        end -= 128;
        if end >= start + 227 && &bytes[end - 227..end - 223] == b"TAG+" {
            end -= 227;
        }
    }

    &bytes[start..end]
}

/// Reads the ID3v1 tag at the end of `bytes` with the fork's reader.
fn read_id3v1(bytes: &[u8]) -> Vec<Tag> {
    assert!(has_id3v1(bytes), "no ID3v1 tag");

    let tail = bytes[bytes.len() - 128..].to_vec();
    let mss = MediaSourceStream::new(Box::new(Cursor::new(tail)), Default::default());

    let mut reader = Id3v1Reader::try_new(mss, MetadataOptions::default()).unwrap();
    reader.read_all().unwrap().revision.media.tags
}

fn assert_new_id3v1(bytes: &[u8]) {
    assert!(has_id3v1(bytes));

    let tag = &bytes[bytes.len() - 128..];

    // The layout of ID3v1.1: title, artist and album are 30 bytes each, NUL padded, then the year
    // (4), the comment (28), a NUL, the track number and the genre.
    assert_eq!(&tag[3..12], b"New Title");
    assert!(tag[12..33].iter().all(|&b| b == 0));
    assert_eq!(&tag[33..43], b"New Artist");
    assert_eq!(&tag[63..72], b"New Album");
    assert_eq!(tag[125], 0);
    assert_eq!(tag[126], 7);
    assert_eq!(tag[127], 17, "Rock");

    let tags = read_id3v1(bytes);
    let std: Vec<_> = tags.iter().filter_map(|t| t.std.clone()).collect();

    for expected in [
        StandardTag::TrackTitle(s("New Title")),
        StandardTag::Artist(s("New Artist")),
        StandardTag::Album(s("New Album")),
        StandardTag::TrackNumber(7),
        StandardTag::Genre(s("Rock")),
    ] {
        assert!(std.contains(&expected), "missing {expected:?} in {std:?}");
    }
}

/// The two files the ID3v1 policy is checked on: the container, and the stream without any tag.
fn subjects() -> [(&'static str, Container, Vec<u8>); 2] {
    [("a.mp3", Container::MpegAudio, mpeg_audio()), ("a.tta", Container::Tta, tta_audio())]
}

#[test]
fn preserve_never_adds_an_id3v1_tag() {
    for (name, container, audio) in subjects() {
        let path = make_file(name, &audio);
        assert_eq!(sniff(&path).unwrap(), container);

        write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Preserve)).unwrap();

        let after = fs::read(&path).unwrap();
        assert!(!has_id3v1(&after), "{name}");
        assert_eq!(&after[..3], b"ID3", "{name}: the ID3v2 tag is still written");
        assert_eq!(core(&after), &audio[..], "{name}");
        cleanup(&path);
    }
}

#[test]
fn preserve_rewrites_an_existing_tag() {
    for (name, _, audio) in subjects() {
        let mut original = audio.clone();
        original.extend_from_slice(&old_id3v1());
        let path = make_file(name, &original);

        write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Preserve)).unwrap();

        let after = fs::read(&path).unwrap();
        assert_new_id3v1(&after);
        assert_eq!(core(&after), &audio[..], "{name}");
        cleanup(&path);
    }
}

#[test]
fn preserve_removes_the_tag_when_there_are_no_tags() {
    for (name, _, audio) in subjects() {
        let mut original = audio.clone();
        original.extend_from_slice(&old_id3v1());
        let path = make_file(name, &original);

        write_file(&path, &[], &[], &opts(Id3v1Policy::Preserve)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), audio, "{name}");
        cleanup(&path);

        // A file without tags is not touched at all.
        let path = make_file(name, &audio);
        write_file(&path, &[], &[], &opts(Id3v1Policy::Preserve)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), audio, "{name}");
        cleanup(&path);
    }
}

#[test]
fn write_adds_a_tag_to_a_file_without_one() {
    for (name, _, audio) in subjects() {
        let path = make_file(name, &audio);

        write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Write)).unwrap();

        let after = fs::read(&path).unwrap();
        assert_new_id3v1(&after);
        assert_eq!(&after[..3], b"ID3", "{name}");
        assert_eq!(core(&after), &audio[..], "{name}: audio bytes changed");

        // Writing again replaces the tag rather than stacking another one.
        let len = after.len();
        write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Write)).unwrap();
        let again = fs::read(&path).unwrap();
        assert_eq!(again.len(), len, "{name}");
        assert_new_id3v1(&again);
        cleanup(&path);
    }
}

#[test]
fn write_replaces_an_existing_plain_or_enhanced_tag() {
    for (name, _, audio) in subjects() {
        for with_enhanced in [false, true] {
            let mut original = audio.clone();
            if with_enhanced {
                original.extend_from_slice(&enhanced_id3v1());
            }
            original.extend_from_slice(&old_id3v1());
            let path = make_file(name, &original);

            write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Write)).unwrap();

            let after = fs::read(&path).unwrap();
            assert_new_id3v1(&after);
            assert!(!contains(&after, b"TAG+"), "{name}: the enhanced tag is gone");
            assert_eq!(core(&after), &audio[..], "{name}");
            cleanup(&path);
        }
    }
}

#[test]
fn write_without_tags_adds_nothing_and_removes_an_existing_tag() {
    for (name, _, audio) in subjects() {
        let path = make_file(name, &audio);
        write_file(&path, &[], &[], &opts(Id3v1Policy::Write)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), audio, "{name}");
        cleanup(&path);

        let mut original = audio.clone();
        original.extend_from_slice(&old_id3v1());
        let path = make_file(name, &original);
        write_file(&path, &[], &[], &opts(Id3v1Policy::Write)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), audio, "{name}");
        cleanup(&path);
    }
}

#[test]
fn remove_drops_plain_and_enhanced_tags_and_adds_none() {
    for (name, _, audio) in subjects() {
        for with_enhanced in [false, true] {
            let mut original = audio.clone();
            if with_enhanced {
                original.extend_from_slice(&enhanced_id3v1());
            }
            original.extend_from_slice(&old_id3v1());
            let path = make_file(name, &original);

            write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Remove)).unwrap();

            let after = fs::read(&path).unwrap();
            assert!(!has_id3v1(&after), "{name}");
            assert!(!contains(&after, b"TAG+"), "{name}");
            assert_eq!(&after[..3], b"ID3", "{name}: the ID3v2 tag is still written");
            assert_eq!(core(&after), &audio[..], "{name}");
            cleanup(&path);
        }

        // Without a tag to remove, the tags only go to the ID3v2 tag.
        let path = make_file(name, &audio);
        write_file(&path, &new_tags(), &[], &opts(Id3v1Policy::Remove)).unwrap();
        let after = fs::read(&path).unwrap();
        assert!(!has_id3v1(&after), "{name}");
        assert_eq!(core(&after), &audio[..], "{name}");
        cleanup(&path);

        // Nothing but the tag to remove: no ID3v2 tag is created.
        let mut original = audio.clone();
        original.extend_from_slice(&old_id3v1());
        let path = make_file(name, &original);
        write_file(&path, &[], &[], &opts(Id3v1Policy::Remove)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), audio, "{name}");
        cleanup(&path);
    }
}

/// An APEv2 tag (with header) holding a title.
fn ape_tag() -> Vec<u8> {
    let tags = vec![std_tag(StandardTag::TrackTitle(s("In APE")))];
    let tag = symphonia_metadata::ape::write::serialize(&tags, &[], &[]);
    assert!(tag.starts_with(APE_MAGIC));
    tag
}

fn ape_opts(remove: bool) -> WriteOptions {
    WriteOptions { remove_ape_from_mpeg: remove, padding: 0, ..Default::default() }
}

#[test]
fn remove_ape_from_mpeg_on_the_ape_fixture() {
    let audio = mpeg_audio();
    let fixture_bytes = fs::read(fixture("mpeg-ape.mp3")).unwrap();
    assert!(has_id3v1(&fixture_bytes));
    let ape = fixture_bytes[audio.len()..fixture_bytes.len() - 128].to_vec();
    assert!(ape.starts_with(APE_MAGIC));

    // false: the APE tag is untouched.
    let path = make_file("ape.mp3", &fixture_bytes);
    write_file(&path, &new_tags(), &[], &ape_opts(false)).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(contains(&after, &ape));
    assert!(has_id3v1(&after));
    cleanup(&path);

    // true: the APE tag is gone, the ID3v1 tag is rewritten and the audio is intact.
    let path = make_file("ape.mp3", &fixture_bytes);
    write_file(&path, &new_tags(), &[], &ape_opts(true)).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(!contains(&after, APE_MAGIC));
    assert_new_id3v1(&after);
    assert_eq!(&after[..3], b"ID3");
    assert_eq!(core(&after), &audio[..]);
    cleanup(&path);

    // true, no tags: only the audio remains (the ID3v1 tag goes away with the tags).
    let path = make_file("ape.mp3", &fixture_bytes);
    write_file(&path, &[], &[], &ape_opts(true)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), audio);
    cleanup(&path);

    // true, ID3v1 removed by policy.
    let path = make_file("ape.mp3", &fixture_bytes);
    let o = WriteOptions { id3v1: Id3v1Policy::Remove, ..ape_opts(true) };
    write_file(&path, &new_tags(), &[], &o).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(!contains(&after, APE_MAGIC));
    assert!(!has_id3v1(&after));
    assert_eq!(core(&after), &audio[..]);
    cleanup(&path);
}

#[test]
fn remove_ape_from_mpeg_without_id3v1_and_with_an_enhanced_tag() {
    let audio = mpeg_audio();
    let ape = ape_tag();

    // APE at the very end of the stream.
    let mut original = audio.clone();
    original.extend_from_slice(&ape);

    let path = make_file("a.mp3", &original);
    write_file(&path, &[], &[], &ape_opts(false)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original, "kept when false");
    cleanup(&path);

    let path = make_file("a.mp3", &original);
    write_file(&path, &[], &[], &ape_opts(true)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), audio);
    cleanup(&path);

    let path = make_file("a.mp3", &original);
    write_file(&path, &new_tags(), &[], &ape_opts(true)).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(!contains(&after, APE_MAGIC));
    assert!(!has_id3v1(&after), "Preserve does not add an ID3v1 tag");
    assert_eq!(core(&after), &audio[..]);
    cleanup(&path);

    // APE, then TAG+, then TAG.
    let mut original = audio.clone();
    original.extend_from_slice(&ape);
    original.extend_from_slice(&enhanced_id3v1());
    original.extend_from_slice(&old_id3v1());

    let path = make_file("a.mp3", &original);
    write_file(&path, &new_tags(), &[], &ape_opts(true)).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(!contains(&after, APE_MAGIC));
    assert!(!contains(&after, b"TAG+"));
    assert_new_id3v1(&after);
    assert_eq!(core(&after), &audio[..]);
    cleanup(&path);
}

#[test]
fn remove_ape_from_mpeg_with_an_appended_id3v2_tag() {
    let audio = mpeg_audio();
    let fixture_bytes = fs::read(fixture("mpeg-appended.mp3")).unwrap();
    assert!(contains(&fixture_bytes, b"3DI"));

    // The stream: audio, appended ID3v2, then an APE tag (and the ID3v1 tag if the fixture has one).
    let (body, v1) = if has_id3v1(&fixture_bytes) {
        fixture_bytes.split_at(fixture_bytes.len() - 128)
    }
    else {
        (&fixture_bytes[..], &[][..])
    };

    let mut original = body.to_vec();
    original.extend_from_slice(&ape_tag());
    original.extend_from_slice(v1);

    for remove in [false, true] {
        let path = make_file("a.mp3", &original);
        write_file(&path, &new_tags(), &[], &ape_opts(remove)).unwrap();

        let after = fs::read(&path).unwrap();
        assert!(!contains(&after, b"3DI"), "the appended tag moved to the front");
        assert_eq!(contains(&after, APE_MAGIC), !remove);
        assert_eq!(&after[..3], b"ID3");

        let stripped = if remove {
            core(&after).to_vec()
        }
        else {
            // The APE tag stays between the audio and the ID3v1 tag.
            let c = core(&after);
            assert!(c.starts_with(&audio));
            assert_eq!(&c[audio.len()..], &ape_tag()[..]);
            c[..audio.len()].to_vec()
        };
        assert_eq!(stripped, audio, "remove={remove}");
        cleanup(&path);
    }
}
