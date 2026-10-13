#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip tests for APEv2 tags on WavPack, Monkey's Audio and Musepack files.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::{Hint, ProbeableMetadata};
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    MetadataOptions, RawTag, RawValue, StandardTag, StandardVisualKey, Tag, Visual,
};
use symphonia_metadata::ape::ApeReader;
use symphonia_tagging::{Container, WriteOptions, sniff, write_file};

const APE_MAGIC: &[u8] = b"APETAGEX";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Copies a fixture to a private temporary directory.
fn work_copy(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-ape-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::copy(fixture(name), &path).unwrap();
    path
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

/// Reads all tags and visuals the fork's own probe finds in an APE tag.
fn read_tags(path: &Path) -> (Vec<Tag>, Vec<Visual>) {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("wv");

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe");

    let mut tags = Vec::new();
    let mut visuals = Vec::new();

    loop {
        if let Some(rev) = format.metadata().current() {
            if rev.info.short_name.starts_with("ape") {
                tags.extend(rev.media.tags.iter().cloned());
                visuals.extend(rev.media.visuals.iter().cloned());
            }
        }

        if format.metadata().is_latest() {
            break;
        }

        format.metadata().pop();
    }

    (tags, visuals)
}

/// Decodes every packet of the first audio track. Returns the number of decoded frames.
fn decode_all(path: &Path) -> usize {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("wv");

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe");

    let track = format.default_track(TrackType::Audio).expect("audio track");
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not an audio track");
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .expect("decoder");

    let mut frames = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => panic!("packet: {e}"),
        };

        if packet.track_id != track_id {
            continue;
        }

        frames += decoder.decode(&packet).expect("decode").frames();
    }

    frames
}

/// The offset of the APE header in a file, or its length if there is none.
fn audio_len(bytes: &[u8]) -> usize {
    bytes.windows(8).position(|w| w == APE_MAGIC).unwrap_or(bytes.len())
}

fn std_tags(tags: &[Tag]) -> Vec<StandardTag> {
    let mut out: Vec<_> = tags.iter().filter_map(|t| t.std.clone()).collect();
    out.sort_by_key(|t| format!("{t:?}"));
    out
}

fn new_tags() -> Vec<Tag> {
    vec![
        Tag::new_std(RawTag::new("Title", "New Title"), StandardTag::TrackTitle(s("New Title"))),
        Tag::new_std(RawTag::new("Artist", "First"), StandardTag::Artist(s("First"))),
        Tag::new_std(RawTag::new("Artist", "Second"), StandardTag::Artist(s("Second"))),
        Tag::new_std(RawTag::new("Track", "3/12"), StandardTag::TrackNumber(3)),
        Tag::new_std(RawTag::new("Track", "3/12"), StandardTag::TrackTotal(12)),
        Tag::new_std(RawTag::new("Disc", "1/2"), StandardTag::DiscNumber(1)),
        Tag::new_std(RawTag::new("Disc", "1/2"), StandardTag::DiscTotal(2)),
        Tag::new_std(RawTag::new("x", "Rock"), StandardTag::Genre(s("Rock"))),
        Tag::new_std(
            RawTag::new("x", "1"),
            StandardTag::MusicBrainzRecordingId(s("11111111-2222-3333-4444-555555555555")),
        ),
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

fn assert_new_tags_read_back(path: &Path) {
    let (tags, visuals) = read_tags(path);
    let std = std_tags(&tags);

    for expected in [
        StandardTag::TrackTitle(s("New Title")),
        StandardTag::Artist(s("First")),
        StandardTag::Artist(s("Second")),
        StandardTag::TrackNumber(3),
        StandardTag::TrackTotal(12),
        StandardTag::DiscNumber(1),
        StandardTag::DiscTotal(2),
        StandardTag::Genre(s("Rock")),
        StandardTag::MusicBrainzTrackId(s("11111111-2222-3333-4444-555555555555")),
    ] {
        assert!(std.contains(&expected), "missing {expected:?} in {std:?}");
    }

    assert!(tags.iter().any(|t| t.std.is_none()
        && t.raw.key == "My Custom Key"
        && matches!(&t.raw.value, RawValue::String(v) if **v == "custom value")));

    assert_eq!(visuals.len(), 1);
    assert_eq!(visuals[0].usage, Some(StandardVisualKey::FrontCover));
    assert_eq!(visuals[0].media_type.as_deref(), Some("image/png"));
    assert_eq!(visuals[0].data, png());
}

#[test]
fn plain_wavpack_round_trip() {
    let path = work_copy("ape-plain.wv");
    let original = fs::read(&path).unwrap();
    let frames = decode_all(&path);
    assert!(frames > 0);

    assert_eq!(sniff(&path).unwrap(), Container::WavPack);
    write_file(&path, &new_tags(), &[front_cover()], &WriteOptions::default()).unwrap();

    let written = fs::read(&path).unwrap();
    assert!(written.len() > original.len());
    assert_eq!(&written[..original.len()], &original[..], "audio bytes changed");
    assert_eq!(&written[written.len() - 32..written.len() - 24], APE_MAGIC);

    assert_new_tags_read_back(&path);
    assert_eq!(decode_all(&path), frames);

    // Writing again replaces the tag rather than stacking another one.
    write_file(&path, &new_tags(), &[front_cover()], &WriteOptions::default()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), written);
}

#[test]
fn pre_tagged_wavpack_keeps_unmanaged_items() {
    let path = work_copy("ape-tagged.wv");
    let original = fs::read(&path).unwrap();
    let audio = audio_len(&original);
    let frames = decode_all(&path);

    let (mut tags, visuals) = read_tags(&path);
    assert_eq!(visuals.len(), 1);
    assert!(
        tags.iter()
            .any(|t| t.raw.key == "Unknown Bin" && matches!(t.raw.value, RawValue::Binary(_)))
    );

    // Edit the title and keep everything else the reader returned.
    for tag in &mut tags {
        if matches!(tag.std, Some(StandardTag::TrackTitle(_))) {
            *tag =
                Tag::new_std(RawTag::new("Title", "Edited"), StandardTag::TrackTitle(s("Edited")));
        }
    }

    write_file(&path, &tags, &visuals, &WriteOptions::default()).unwrap();

    let written = fs::read(&path).unwrap();
    assert_eq!(&written[..audio], &original[..audio], "audio bytes changed");

    let (after, after_visuals) = read_tags(&path);
    assert!(std_tags(&after).contains(&StandardTag::TrackTitle(s("Edited"))));
    assert!(!std_tags(&after).contains(&StandardTag::TrackTitle(s("Old Title"))));
    assert!(std_tags(&after).contains(&StandardTag::Artist(s("Old Artist"))));
    assert!(std_tags(&after).contains(&StandardTag::TrackNumber(2)));
    assert!(std_tags(&after).contains(&StandardTag::TrackTotal(9)));

    let bin = |tags: &[Tag]| {
        tags.iter().find(|t| t.raw.key == "Unknown Bin").map(|t| t.raw.value.clone())
    };
    assert_eq!(bin(&after), bin(&tags));
    assert!(after.iter().any(|t| t.raw.key == "Custom Field"));

    assert_eq!(after_visuals.len(), 1);
    assert_eq!(after_visuals[0].data, visuals[0].data);
    assert_eq!(after_visuals[0].usage, Some(StandardVisualKey::FrontCover));

    assert_eq!(decode_all(&path), frames);
}

#[test]
fn id3v1_after_the_ape_tag_is_kept_in_place() {
    let path = work_copy("ape-tagged-id3v1.wv");
    let original = fs::read(&path).unwrap();
    let audio = audio_len(&original);
    let id3v1 = original[original.len() - 128..].to_vec();
    assert_eq!(&id3v1[..3], b"TAG");

    write_file(&path, &new_tags(), &[front_cover()], &WriteOptions::default()).unwrap();

    let written = fs::read(&path).unwrap();
    assert_eq!(&written[..audio], &original[..audio], "audio bytes changed");
    assert_eq!(&written[written.len() - 128..], &id3v1[..], "ID3v1 tag changed");
    let footer = &written[written.len() - 128 - 32..written.len() - 128];
    assert_eq!(&footer[..8], APE_MAGIC);

    assert_new_tags_read_back(&path);
    assert_eq!(
        written.windows(8).filter(|w| *w == APE_MAGIC).count(),
        2,
        "exactly one APE tag (header + footer)"
    );
}

#[test]
fn empty_tags_remove_the_ape_tag() {
    let plain = fs::read(fixture("ape-plain.wv")).unwrap();

    let path = work_copy("ape-tagged.wv");
    write_file(&path, &[], &[], &WriteOptions::default()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), plain);
    assert!(decode_all(&path) > 0);

    // An ID3v1 tag stays.
    let path = work_copy("ape-tagged-id3v1.wv");
    let id3v1 = fs::read(&path).unwrap();
    let id3v1 = id3v1[id3v1.len() - 128..].to_vec();
    write_file(&path, &[], &[], &WriteOptions::default()).unwrap();
    let mut expected = plain.clone();
    expected.extend_from_slice(&id3v1);
    assert_eq!(fs::read(&path).unwrap(), expected);

    // Nothing to remove, nothing to add.
    let path = work_copy("ape-plain.wv");
    write_file(&path, &[], &[], &WriteOptions::default()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), plain);
}

/// A synthetic file: an identifying header followed by junk standing in for audio.
fn synthetic(magic: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut v = magic.to_vec();
    v.extend((0..300u32).map(|i| (i * 7 % 251) as u8));
    v.extend_from_slice(tail);
    v
}

fn id3v1_tag() -> Vec<u8> {
    let mut t = b"TAG".to_vec();
    t.resize(128, b' ');
    t
}

/// Parses the APE tag found in `bytes` (starting at its header) with the fork's reader.
fn parse_ape(bytes: &[u8]) -> (Vec<Tag>, Vec<Visual>) {
    let start = bytes.windows(8).position(|w| w == APE_MAGIC).expect("an APE tag");
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(bytes[start..].to_vec())), Default::default());
    let mut reader = ApeReader::try_probe_new(mss, MetadataOptions::default()).unwrap();
    let rev = reader.read_all().unwrap().revision;
    (rev.media.tags, rev.media.visuals)
}

fn synthetic_case(name: &str, magic: &[u8], container: Container) {
    let dir =
        std::env::temp_dir().join(format!("symphonia-tagging-ape-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);

    let audio = synthetic(magic, &[]);

    for with_id3v1 in [false, true] {
        let tail = if with_id3v1 { id3v1_tag() } else { Vec::new() };

        // Start with an APEv1 tag (items "Title=Old", no header) so replacing a v1 tag is covered.
        let mut v1 = Vec::new();
        let item = {
            let mut i = 3u32.to_le_bytes().to_vec();
            i.extend_from_slice(&0u32.to_le_bytes());
            i.extend_from_slice(b"Title\0Old");
            i
        };
        v1.extend_from_slice(&item);
        v1.extend_from_slice(APE_MAGIC);
        v1.extend_from_slice(&1000u32.to_le_bytes());
        v1.extend_from_slice(&((item.len() + 32) as u32).to_le_bytes());
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&[0; 12]);

        let mut original = audio.clone();
        original.extend_from_slice(&v1);
        original.extend_from_slice(&tail);
        fs::write(&path, &original).unwrap();

        assert_eq!(sniff(&path).unwrap(), container);
        write_file(&path, &new_tags(), &[front_cover()], &WriteOptions::default()).unwrap();

        let written = fs::read(&path).unwrap();
        assert_eq!(&written[..audio.len()], &audio[..], "audio bytes changed");
        assert!(written.ends_with(&tail));

        let tag_end = written.len() - tail.len();
        assert_eq!(&written[tag_end - 32..tag_end - 24], APE_MAGIC);
        assert_eq!(
            &written[audio.len()..audio.len() + 8],
            APE_MAGIC,
            "tag starts right after the audio"
        );

        let (tags, visuals) = parse_ape(&written[audio.len()..]);
        assert!(std_tags(&tags).contains(&StandardTag::TrackTitle(s("New Title"))));
        assert!(!tags.iter().any(|t| matches!(&t.raw.value, RawValue::String(v) if **v == "Old")));
        assert!(std_tags(&tags).contains(&StandardTag::TrackNumber(3)));
        assert_eq!(visuals.len(), 1);
        assert_eq!(visuals[0].data, png());

        // Removing the tag restores the audio and the ID3v1 tag exactly.
        write_file(&path, &[], &[], &WriteOptions::default()).unwrap();
        let mut expected = audio.clone();
        expected.extend_from_slice(&tail);
        assert_eq!(fs::read(&path).unwrap(), expected);
    }
}

#[test]
fn synthetic_monkeys_audio() {
    synthetic_case("synthetic.ape", b"MAC \x96\x0f\x00\x00", Container::MonkeysAudio);
}

#[test]
fn synthetic_musepack() {
    synthetic_case("synthetic.mpc", b"MPCK", Container::Musepack);
}
