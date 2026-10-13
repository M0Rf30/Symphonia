// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the ID3v2 options of [`WriteOptions`]: text encoding, the ID3v2.3 join separator, the
//! iTunes grouping and ratings.
//!
//! The files are written from `mpeg-plain.mp3` (no tags) and `mpeg-v23.mp3` (an ID3v2.3 tag with a
//! `POPM` frame of `user@example.org`), and the tags are read back with the fork's own probe.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawTag, RawTagSubField, StandardTag, Tag};
use symphonia_tagging::{Id3Encoding, Id3v2Version, WriteOptions, write_file};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Copies a fixture to a private temporary directory.
fn work_copy(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-id3opts-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::copy(fixture(name), &path).unwrap();
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

fn rating_with_email(ppm: u32, email: &str) -> Tag {
    Tag::new_std(
        RawTag::new_with_sub_fields(
            "POPM",
            0u8,
            vec![RawTagSubField::new("EMAIL", email)].into_boxed_slice(),
        ),
        StandardTag::Rating(ppm),
    )
}

fn opts(version: Id3v2Version) -> WriteOptions {
    WriteOptions { id3v2_version: version, padding: 0, ..Default::default() }
}

fn open(path: &Path) -> Box<dyn FormatReader + 'static> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe")
}

/// The tags of all metadata revisions the probe finds (the newest first).
fn read_tags(path: &Path) -> Vec<Tag> {
    let mut format = open(path);
    let mut tags = Vec::new();

    loop {
        if let Some(rev) = format.metadata().current() {
            tags.extend(rev.media.tags.iter().cloned());
        }

        if format.metadata().is_latest() {
            break;
        }
        format.metadata().pop();
    }

    tags
}

fn find<'a>(tags: &'a [Tag], key: &str) -> Vec<&'a Tag> {
    tags.iter().filter(|t| t.raw.key == key).collect()
}

fn sub_field(tag: &Tag, name: &str) -> Option<String> {
    tag.raw
        .sub_fields
        .as_ref()?
        .iter()
        .find(|f| f.field == name)
        .map(|f| f.value.to_string())
}

fn txxx_descriptions(tags: &[Tag]) -> Vec<String> {
    find(tags, "TXXX").iter().filter_map(|t| sub_field(t, "DESCRIPTION")).collect()
}

/// The (version, frames) of the ID3v2 tag at the start of the file.
fn leading_frames(path: &Path) -> (u8, Vec<([u8; 4], Vec<u8>)>) {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..3], b"ID3");
    let major = bytes[3];
    let size = bytes[6..10].iter().fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f));
    let body = &bytes[10..10 + size];

    let mut frames = Vec::new();
    let mut pos = 0;

    while pos + 10 <= body.len() && body[pos] != 0 {
        let id: [u8; 4] = body[pos..pos + 4].try_into().unwrap();
        let len_bytes = &body[pos + 4..pos + 8];
        let len = if major == 4 {
            len_bytes.iter().fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f))
        }
        else {
            u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize
        };
        frames.push((id, body[pos + 10..pos + 10 + len].to_vec()));
        pos += 10 + len;
    }

    (major, frames)
}

fn frame_data(path: &Path, id: &[u8; 4]) -> Option<Vec<u8>> {
    leading_frames(path).1.into_iter().find(|(i, _)| i == id).map(|(_, d)| d)
}

fn popm_frames(path: &Path) -> Vec<Vec<u8>> {
    leading_frames(path).1.into_iter().filter(|(i, _)| i == b"POPM").map(|(_, d)| d).collect()
}

fn popm(email: &str, rating: u8, counter: &[u8]) -> Vec<u8> {
    [email.as_bytes(), &[0, rating], counter].concat()
}

/// The bytes of the stream (everything but the tags).
fn stream_of(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    let start = if &bytes[..3] == b"ID3" {
        10 + bytes[6..10].iter().fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f))
    }
    else {
        0
    };
    let mut end = bytes.len();
    if end >= 128 && &bytes[end - 128..end - 125] == b"TAG" {
        end -= 128;
    }
    bytes[start..end].to_vec()
}

#[test]
fn text_encodings_are_written_and_round_trip() {
    let title = "\u{41a}\u{438}\u{43d}\u{43e} \u{20ac}";

    // (version, encoding, expected encoding byte for ASCII text, for non-Latin-1 text).
    let cases = [
        (Id3v2Version::V4, Id3Encoding::Auto, 3, 3),
        (Id3v2Version::V3, Id3Encoding::Auto, 0, 1),
        (Id3v2Version::V4, Id3Encoding::Utf8, 3, 3),
        (Id3v2Version::V3, Id3Encoding::Utf8, 1, 1),
        (Id3v2Version::V4, Id3Encoding::Utf16, 1, 1),
        (Id3v2Version::V3, Id3Encoding::Utf16, 1, 1),
        (Id3v2Version::V4, Id3Encoding::Latin1, 0, 1),
        (Id3v2Version::V3, Id3Encoding::Latin1, 0, 1),
    ];

    let expected_audio = stream_of(&fixture("mpeg-plain.mp3"));

    for (version, encoding, ascii_enc, wide_enc) in cases {
        for (text, expected_enc) in [("Plain", ascii_enc), (title, wide_enc)] {
            let path = work_copy("mpeg-plain.mp3");
            let o = WriteOptions { id3_encoding: encoding, ..opts(version) };
            let tags = [std_tag(StandardTag::TrackTitle(s(text)))];
            write_file(&path, &tags, &[], &o).unwrap();

            let (major, _) = leading_frames(&path);
            assert_eq!(major, if version == Id3v2Version::V3 { 3 } else { 4 });
            assert_eq!(
                frame_data(&path, b"TIT2").unwrap()[0],
                expected_enc,
                "{version:?} {encoding:?} {text}"
            );

            let read = read_tags(&path);
            let title_tag = find(&read, "TIT2");
            assert_eq!(title_tag.len(), 1);
            assert_eq!(title_tag[0].raw.value.to_string(), text);
            assert_eq!(title_tag[0].std, Some(StandardTag::TrackTitle(s(text))));

            assert_eq!(stream_of(&path), expected_audio);
            cleanup(&path);
        }
    }
}

#[test]
fn utf16_text_round_trips_in_other_frames_and_multiple_values() {
    for version in [Id3v2Version::V3, Id3v2Version::V4] {
        let path = work_copy("mpeg-plain.mp3");
        let o = WriteOptions { id3_encoding: Id3Encoding::Utf16, ..opts(version) };
        let tags = [
            std_tag(StandardTag::Artist(s("\u{41a}\u{438}\u{43d}\u{43e}"))),
            std_tag(StandardTag::Artist(s("\u{20ac}uro"))),
            std_tag(StandardTag::Comment(s("c\u{43e}mment"))),
            Tag::new(RawTag::new("MYKEY", "va\u{20ac}")),
        ];
        write_file(&path, &tags, &[], &o).unwrap();

        for id in [b"TPE1", b"COMM", b"TXXX"] {
            assert_eq!(frame_data(&path, id).unwrap()[0], 1);
        }

        let read = read_tags(&path);
        let artists: Vec<String> =
            find(&read, "TPE1").iter().map(|t| t.raw.value.to_string()).collect();

        if version == Id3v2Version::V4 {
            assert_eq!(artists, vec!["\u{41a}\u{438}\u{43d}\u{43e}", "\u{20ac}uro"]);
        }
        else {
            assert_eq!(artists, vec!["\u{41a}\u{438}\u{43d}\u{43e}/\u{20ac}uro"]);
        }

        assert!(
            read.iter()
                .any(|t| t.std == Some(StandardTag::Comment(s("c\u{43e}mment"))))
        );
        assert_eq!(find(&read, "TXXX")[0].raw.value.to_string(), "va\u{20ac}");
        cleanup(&path);
    }
}

#[test]
fn v23_join_separator() {
    let tags = [
        std_tag(StandardTag::Artist(s("Artist One"))),
        std_tag(StandardTag::Artist(s("Artist Two"))),
    ];

    for (join, expected) in [("; ", "Artist One; Artist Two"), ("", "Artist OneArtist Two")] {
        let path = work_copy("mpeg-plain.mp3");
        let o = WriteOptions { id3v23_join_with: join.to_string(), ..opts(Id3v2Version::V3) };
        write_file(&path, &tags, &[], &o).unwrap();

        let read = read_tags(&path);
        let artists = find(&read, "TPE1");
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].raw.value.to_string(), expected);
        cleanup(&path);
    }

    // The default is `/`, and ID3v2.4 keeps separate values.
    let path = work_copy("mpeg-plain.mp3");
    write_file(&path, &tags, &[], &opts(Id3v2Version::V3)).unwrap();
    assert_eq!(find(&read_tags(&path), "TPE1")[0].raw.value.to_string(), "Artist One/Artist Two");

    let o = WriteOptions { id3v23_join_with: "; ".to_string(), ..opts(Id3v2Version::V4) };
    write_file(&path, &tags, &[], &o).unwrap();
    let read = read_tags(&path);
    assert_eq!(find(&read, "TPE1").len(), 2);
    cleanup(&path);
}

#[test]
fn itunes_grouping_writes_grp1_and_tit1() {
    let tags = [
        std_tag(StandardTag::Grouping(s("My Grouping"))),
        std_tag(StandardTag::Work(s("My Work"))),
    ];

    for version in [Id3v2Version::V3, Id3v2Version::V4] {
        let path = work_copy("mpeg-plain.mp3");
        let o = WriteOptions { itunes_grouping: true, ..opts(version) };
        write_file(&path, &tags, &[], &o).unwrap();

        let (_, frames) = leading_frames(&path);
        let ids: Vec<&[u8; 4]> = frames.iter().map(|(i, _)| i).collect();
        assert!(ids.contains(&b"GRP1"));
        assert!(ids.contains(&b"TIT1"));
        assert!(!ids.contains(&b"TXXX"));

        let read = read_tags(&path);
        let grp1 = find(&read, "GRP1");
        let tit1 = find(&read, "TIT1");
        assert_eq!(grp1.len(), 1);
        assert_eq!(tit1.len(), 1);
        assert_eq!(grp1[0].raw.value.to_string(), "My Grouping");
        assert_eq!(tit1[0].raw.value.to_string(), "My Work");
        assert!(txxx_descriptions(&read).is_empty());
        cleanup(&path);

        // Without the option: TIT1 holds the grouping and the work is a TXXX frame.
        let path = work_copy("mpeg-plain.mp3");
        write_file(&path, &tags, &[], &opts(version)).unwrap();
        let read = read_tags(&path);
        assert!(find(&read, "GRP1").is_empty());
        assert_eq!(find(&read, "TIT1")[0].raw.value.to_string(), "My Grouping");
        assert_eq!(txxx_descriptions(&read), vec!["WORK".to_string()]);
        cleanup(&path);
    }
}

/// The `POPM` frame of the v2.3 fixture.
fn fixture_popm() -> (Vec<u8>, Vec<u8>) {
    let popm = popm_frames(&fixture("mpeg-v23.mp3"));
    assert_eq!(popm.len(), 1);
    let frame = popm[0].clone();
    assert!(frame.starts_with(b"user@example.org\0"));
    // The rating and counter that follow the owner.
    let counter = frame["user@example.org".len() + 2..].to_vec();
    (frame, counter)
}

#[test]
fn default_options_keep_popm_and_ignore_rating() {
    let (frame, _) = fixture_popm();

    let path = work_copy("mpeg-v23.mp3");
    let tags = [std_tag(StandardTag::TrackTitle(s("T"))), std_tag(StandardTag::Rating(1_000_000))];
    write_file(&path, &tags, &[], &opts(Id3v2Version::Preserve)).unwrap();

    assert_eq!(popm_frames(&path), vec![frame.clone()]);

    // The preserved frame is read as a rating.
    let read = read_tags(&path);
    let popm = find(&read, "POPM");
    assert_eq!(popm.len(), 1);
    assert_eq!(sub_field(popm[0], "EMAIL").as_deref(), Some("user@example.org"));
    assert!(matches!(popm[0].std, Some(StandardTag::Rating(_))));
    cleanup(&path);
}

#[test]
fn rating_is_written_replaced_and_removed() {
    let (_, counter) = fixture_popm();
    let email = "user@example.org";

    let o = WriteOptions { rating_email: Some(email.to_string()), ..opts(Id3v2Version::Preserve) };

    // Written over the existing frame, with the counter carried over.
    let path = work_copy("mpeg-v23.mp3");
    let tags = [std_tag(StandardTag::Rating(1_000_000))];
    write_file(&path, &tags, &[], &o).unwrap();
    assert_eq!(popm_frames(&path), vec![popm(email, 255, &counter)]);

    let read = read_tags(&path);
    let popm_tags = find(&read, "POPM");
    assert_eq!(popm_tags.len(), 1);
    assert_eq!(popm_tags[0].std, Some(StandardTag::Rating(1_000_000)));
    assert_eq!(sub_field(popm_tags[0], "EMAIL").as_deref(), Some(email));

    // Changed (rounded to the nearest of 256 steps).
    let tags = [std_tag(StandardTag::Rating(600_000))];
    write_file(&path, &tags, &[], &o).unwrap();
    assert_eq!(popm_frames(&path), vec![popm(email, 153, &counter)]);

    // Removed by omitting the rating.
    write_file(&path, &[std_tag(StandardTag::TrackTitle(s("T")))], &[], &o).unwrap();
    assert!(popm_frames(&path).is_empty());
    cleanup(&path);

    // No rating and no existing frame for the email: nothing is written.
    let path = work_copy("mpeg-plain.mp3");
    write_file(&path, &[std_tag(StandardTag::TrackTitle(s("T")))], &[], &o).unwrap();
    assert!(popm_frames(&path).is_empty());

    // A new frame has no counter.
    write_file(&path, &[std_tag(StandardTag::Rating(0))], &[], &o).unwrap();
    assert_eq!(popm_frames(&path), vec![popm(email, 0, &[])]);
    cleanup(&path);
}

#[test]
fn ratings_of_other_users_are_preserved_and_raw_email_is_used() {
    let (fixture_frame, counter) = fixture_popm();

    // Another user's rating is kept when the configured email is a different one.
    let path = work_copy("mpeg-v23.mp3");
    let o = WriteOptions {
        rating_email: Some("me@example.org".to_string()),
        ..opts(Id3v2Version::Preserve)
    };
    write_file(&path, &[std_tag(StandardTag::Rating(500_000))], &[], &o).unwrap();
    assert_eq!(
        popm_frames(&path),
        vec![popm("me@example.org", 128, &[]), fixture_frame.clone()]
    );

    // Writing again replaces only the configured user's frame.
    write_file(&path, &[std_tag(StandardTag::Rating(1_000_000))], &[], &o).unwrap();
    assert_eq!(
        popm_frames(&path),
        vec![popm("me@example.org", 255, &[]), fixture_frame.clone()]
    );
    cleanup(&path);

    // A rating that has an email in its raw tag is written for that email, and the counter of
    // the existing frame of that email is carried over.
    let path = work_copy("mpeg-v23.mp3");
    let tags = [
        std_tag(StandardTag::Rating(1_000_000)),
        rating_with_email(200_000, "user@example.org"),
    ];
    write_file(&path, &tags, &[], &o).unwrap();
    assert_eq!(
        popm_frames(&path),
        vec![popm("me@example.org", 255, &[]), popm("user@example.org", 51, &counter)]
    );

    // The ratings that are read back from the file are written unchanged.
    let read = read_tags(&path);
    write_file(&path, &read, &[], &o).unwrap();
    let after = popm_frames(&path);
    assert_eq!(after.len(), 2);
    assert!(after.contains(&popm("me@example.org", 255, &[])));
    assert!(after.contains(&popm("user@example.org", 51, &counter)));

    // The rating of a user that is neither the configured one nor written is not touched.
    let without: Vec<Tag> = read
        .into_iter()
        .filter(|t| sub_field(t, "EMAIL").as_deref() != Some("user@example.org"))
        .collect();
    write_file(&path, &without, &[], &o).unwrap();
    assert_eq!(popm_frames(&path).len(), 2);

    // The configured user's rating is removed when it is left out.
    let without_me: Vec<Tag> = without
        .into_iter()
        .filter(|t| sub_field(t, "EMAIL").as_deref() != Some("me@example.org"))
        .collect();
    write_file(&path, &without_me, &[], &o).unwrap();
    assert_eq!(popm_frames(&path).len(), 1);
    assert!(popm_frames(&path)[0].starts_with(b"user@example.org\0"));
    cleanup(&path);
}

#[test]
fn rating_survives_a_version_change() {
    let (_, counter) = fixture_popm();

    let path = work_copy("mpeg-v23.mp3");
    let o = WriteOptions {
        rating_email: Some("user@example.org".to_string()),
        ..opts(Id3v2Version::V4)
    };
    write_file(&path, &[std_tag(StandardTag::Rating(1_000_000))], &[], &o).unwrap();
    assert_eq!(leading_frames(&path).0, 4);
    assert_eq!(popm_frames(&path), vec![popm("user@example.org", 255, &counter)]);
    cleanup(&path);
}
