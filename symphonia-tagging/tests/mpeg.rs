// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip tests for the tags of MPEG audio streams.
//!
//! The fixtures (`mpeg-*.mp3`) share the same 0.5 s mono MPEG-2 layer III stream (`mpeg-plain.mp3`,
//! 1768 bytes) and were made with ffmpeg and mutagen. They carry:
//!
//! * `mpeg-v23.mp3`: ID3v2.3 (APIC, PRIV, POPM, GEOB, UFID, TXXX, ...; 200 bytes of padding) and
//!   ID3v1.
//! * `mpeg-v24.mp3`: ID3v2.4 with PRIV, POPM, SYLT, CHAP and CTOC.
//! * `mpeg-stacked.mp3`: a v2.3 tag followed by a v2.4 tag, each with a PRIV frame.
//! * `mpeg-appended.mp3`: an ID3v2.4 tag with a footer (and a PRIV frame) appended to the stream,
//!   followed by ID3v1.
//! * `mpeg-ape.mp3`: APEv2 followed by ID3v1.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    MetadataOptions, RawTag, RawTagSubField, StandardTag, StandardVisualKey, Tag, Visual,
};
use symphonia_tagging::{Container, Error, Id3v2Version, WriteOptions, sniff, write_file};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// The bytes of the MPEG stream shared by the fixtures.
fn audio() -> Vec<u8> {
    fs::read(fixture("mpeg-plain.mp3")).unwrap()
}

/// Copies a fixture to a private temporary directory.
fn work_copy(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-mpeg-{}-{}",
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

fn cover(data: &[u8]) -> Visual {
    Visual {
        media_type: Some("image/png".to_string()),
        dimensions: None,
        color_mode: None,
        usage: Some(StandardVisualKey::FrontCover),
        tags: Vec::new(),
        data: data.to_vec().into_boxed_slice(),
    }
}

fn opts(version: Id3v2Version) -> WriteOptions {
    WriteOptions { id3v2_version: version, padding: 0 }
}

fn new_tags() -> Vec<Tag> {
    vec![
        std_tag(StandardTag::TrackTitle(s("New Title"))),
        std_tag(StandardTag::Artist(s("New Artist"))),
        std_tag(StandardTag::Album(s("New Album \u{65e5}\u{672c}"))),
        std_tag(StandardTag::TrackNumber(5)),
        std_tag(StandardTag::TrackTotal(12)),
        std_tag(StandardTag::RecordingDate(s("2020-02-03"))),
        std_tag(StandardTag::Genre(s("Jazz"))),
        std_tag(StandardTag::Comment(s("New comment"))),
        Tag::new(RawTag::new("MYKEY", "free-form value")),
    ]
}

fn std_set(tags: &[Tag]) -> BTreeSet<String> {
    tags.iter().filter_map(|t| t.std.as_ref()).map(|t| format!("{t:?}")).collect()
}

struct ReadBack {
    /// Distinct standard tags, over all metadata revisions.
    std: BTreeSet<String>,
    /// The raw keys of all tags (the frame IDs of tags that have no standard tag).
    raw_keys: BTreeSet<String>,
    /// The (description, value) of all TXXX tags.
    txxx: BTreeSet<(String, String)>,
    visuals: Vec<(Option<StandardVisualKey>, Vec<u8>)>,
}

fn open(path: &Path) -> Box<dyn symphonia::core::formats::FormatReader + 'static> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe")
}

/// Reads the tags and visuals the fork's own probe finds.
fn read_back(path: &Path) -> ReadBack {
    let mut format = open(path);

    let mut out = ReadBack {
        std: BTreeSet::new(),
        raw_keys: BTreeSet::new(),
        txxx: BTreeSet::new(),
        visuals: Vec::new(),
    };

    loop {
        if let Some(rev) = format.metadata().current() {
            for tag in &rev.media.tags {
                if let Some(t) = &tag.std {
                    out.std.insert(format!("{t:?}"));
                }
                out.raw_keys.insert(tag.raw.key.clone());

                if tag.raw.key == "TXXX" {
                    let desc = tag
                        .raw
                        .sub_fields
                        .as_ref()
                        .and_then(|f| f.iter().find(|f| f.field == "DESCRIPTION"))
                        .map(|f| f.value.to_string())
                        .unwrap_or_default();
                    out.txxx.insert((desc, tag.raw.value.to_string()));
                }
            }
            for v in &rev.media.visuals {
                let key = (v.usage, v.data.to_vec());
                if !out.visuals.contains(&key) {
                    out.visuals.push(key);
                }
            }
        }

        if format.metadata().is_latest() {
            break;
        }
        format.metadata().pop();
    }

    out
}

/// The bytes of all audio packets.
fn audio_packets(path: &Path) -> Vec<u8> {
    let mut format = open(path);
    let mut bytes = Vec::new();

    loop {
        match format.next_packet() {
            Ok(Some(packet)) => bytes.extend_from_slice(&packet.data),
            Ok(None) => break,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => panic!("packet: {e}"),
        }
    }

    bytes
}

/// Decodes every packet of the first audio track. Returns the number of decoded frames.
fn decode_all(path: &Path) -> usize {
    let mut format = open(path);

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

/// The lengths of the ID3v2 tags at the start of a file, and the major version of the first.
fn leading_tags(bytes: &[u8]) -> (Vec<usize>, Option<u8>) {
    let mut lens = Vec::new();
    let mut pos = 0;
    let mut version = None;

    while bytes.len() >= pos + 10 && &bytes[pos..pos + 3] == b"ID3" {
        let size =
            bytes[pos + 6..pos + 10].iter().fold(0usize, |a, &b| (a << 7) | usize::from(b & 0x7f));
        let footer = if bytes[pos + 3] == 4 && bytes[pos + 5] & 0x10 != 0 { 10 } else { 0 };
        version.get_or_insert(bytes[pos + 3]);
        lens.push(10 + size + footer);
        pos += 10 + size + footer;
    }

    (lens, version)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_id3v1(bytes: &[u8]) -> bool {
    bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG"
}

#[test]
fn v23_with_v1_apic_priv_popm_is_rewritten_and_unmanaged_frames_are_kept() {
    let path = work_copy("mpeg-v23.mp3");
    let before = fs::read(&path).unwrap();
    let (old_lens, version) = leading_tags(&before);
    assert_eq!(version, Some(3));
    assert!(is_id3v1(&before));

    // The old tags are read back.
    let old = read_back(&path);
    assert!(old.std.contains(&format!("{:?}", StandardTag::TrackTitle(s("Old Title")))));
    assert!(old.std.contains(&format!(
        "{:?}",
        StandardTag::MusicBrainzRecordingId(s("11111111-2222-3333-4444-555555555555"))
    )));
    assert_eq!(old.visuals.len(), 1);

    let frames_before = decode_all(&path);
    assert!(frames_before > 0);

    let tags = new_tags();
    // The default padding lets the new tag take the size of the old one.
    let default_opts =
        WriteOptions { id3v2_version: Id3v2Version::Preserve, ..WriteOptions::default() };
    write_file(&path, &tags, &[cover(b"\x89PNG-new-cover")], &default_opts).unwrap();

    let after = fs::read(&path).unwrap();
    let (lens, version) = leading_tags(&after);

    // The version is preserved, and there is one tag.
    assert_eq!(version, Some(3));
    assert_eq!(lens.len(), 1);
    // The new tag fits in the old one: the size is reused and the audio did not move.
    assert_eq!(lens[0], old_lens[0]);
    assert_eq!(&after[lens[0]..lens[0] + audio().len()], &audio()[..]);

    // The ID3v1 tag was rewritten, not added twice.
    assert!(is_id3v1(&after));
    assert_eq!(after.len(), before.len());
    assert_eq!(&after[after.len() - 128 + 3..after.len() - 128 + 12], b"New Title");
    assert_ne!(&after[after.len() - 128..], &before[before.len() - 128..]);

    let read = read_back(&path);
    // Only the new tags are read. The new ones.
    for tag in std_set(&tags) {
        assert!(read.std.contains(&tag), "missing {tag}");
    }
    assert_eq!(read.txxx.iter().filter(|(d, _)| d == "MYKEY").count(), 1);
    // The old ones are gone: the old v1 tag is replaced and the old v2 frames are dropped.
    assert!(!read.std.contains(&format!("{:?}", StandardTag::TrackTitle(s("Old Title")))));
    assert!(!read.std.contains(&format!("{:?}", StandardTag::Album(s("Old Album")))));
    assert!(!read.txxx.iter().any(|(d, _)| d == "OLDKEY"));
    assert!(!read.std.contains(&format!(
        "{:?}",
        StandardTag::MusicBrainzRecordingId(s("11111111-2222-3333-4444-555555555555"))
    )));
    // The picture was replaced.
    assert_eq!(
        read.visuals,
        vec![(Some(StandardVisualKey::FrontCover), b"\x89PNG-new-cover".to_vec())]
    );

    // The unmanaged frames are untouched.
    for needle in [
        &b"com.example.private"[..],
        b"\x01\x02\x03\xff\x00\xfe",
        b"user@example.org",
        b"application/x-test",
        b"f.bin",
    ] {
        assert!(
            contains(&after[..lens[0]], needle),
            "{:?} is missing",
            String::from_utf8_lossy(needle)
        );
    }
    assert!(read.raw_keys.contains("PRIV"));
    assert!(read.raw_keys.contains("POPM"));
    assert!(read.raw_keys.contains("GEOB"));

    // The audio is the same.
    assert_eq!(decode_all(&path), frames_before);
    cleanup(&path);
}

#[test]
fn v24_tag_keeps_its_version_and_unmanaged_frames() {
    let path = work_copy("mpeg-v24.mp3");
    let before = fs::read(&path).unwrap();
    assert_eq!(leading_tags(&before).1, Some(4));
    assert!(!is_id3v1(&before), "a v1 tag is never added");

    let tags = new_tags();
    write_file(&path, &tags, &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    let (lens, version) = leading_tags(&after);
    assert_eq!(version, Some(4));
    assert_eq!(lens.len(), 1);
    assert_eq!(&after[lens[0]..], &audio()[..]);
    assert!(!is_id3v1(&after));

    for needle in [
        &b"PRIV"[..],
        b"com.example.private24",
        b"POPM",
        b"SYLT",
        b"CHAP",
        b"CTOC",
        b"Chapter One",
        b"Contents",
    ] {
        assert!(
            contains(&after[..lens[0]], needle),
            "{:?} is missing",
            String::from_utf8_lossy(needle)
        );
    }
    assert!(!contains(&after[..lens[0]], b"Old Title 24"));

    // The tags are read back exactly (v2.4 keeps multiple values).
    let read = read_back(&path);
    for tag in std_set(&tags) {
        assert!(read.std.contains(&tag), "missing {tag}");
    }
    assert!(!read.std.contains(&format!("{:?}", StandardTag::Album(s("Old Album 24")))));
    assert!(read.raw_keys.contains("PRIV"));
    cleanup(&path);
}

#[test]
fn multiple_values_round_trip_in_v24() {
    let path = work_copy("mpeg-plain.mp3");
    let tags = vec![
        std_tag(StandardTag::Artist(s("Artist A"))),
        std_tag(StandardTag::Artist(s("Artist B"))),
        std_tag(StandardTag::Genre(s("Rock"))),
        std_tag(StandardTag::Genre(s("Pop"))),
    ];

    write_file(&path, &tags, &[], &opts(Id3v2Version::V4)).unwrap();
    assert_eq!(read_back(&path).std, std_set(&tags));
    cleanup(&path);
}

#[test]
fn many_standard_tags_round_trip() {
    let mut tags = vec![
        std_tag(StandardTag::AlbumArtist(s("Album Artist"))),
        std_tag(StandardTag::DiscNumber(1)),
        std_tag(StandardTag::DiscTotal(2)),
        std_tag(StandardTag::OriginalReleaseDate(s("1999-01-02"))),
        std_tag(StandardTag::Lyrics(s("la\nla"))),
        std_tag(StandardTag::Composer(s("Composer"))),
        std_tag(StandardTag::Bpm(120)),
        std_tag(StandardTag::CompilationFlag(true)),
        std_tag(StandardTag::MusicBrainzRecordingId(s("11111111-2222-3333-4444-555555555555"))),
        std_tag(StandardTag::MusicBrainzAlbumId(s("aaaaaaaa-2222-3333-4444-555555555555"))),
        std_tag(StandardTag::MusicBrainzReleaseTrackId(s("bbbbbbbb-2222-3333-4444-555555555555"))),
        std_tag(StandardTag::Arranger(s("Arranger"))),
        std_tag(StandardTag::Producer(s("Producer"))),
        std_tag(StandardTag::Engineer(s("Engineer"))),
        std_tag(StandardTag::Grouping(s("Grouping"))),
        std_tag(StandardTag::SortArtist(s("Artist, The"))),
        std_tag(StandardTag::SortAlbumArtist(s("Album Artist, The"))),
        std_tag(StandardTag::SortAlbum(s("Album, The"))),
        std_tag(StandardTag::SortTrackTitle(s("Title, The"))),
        std_tag(StandardTag::ReplayGainTrackGain(s("-6.50 dB"))),
        std_tag(StandardTag::IdentIsrc(s("USRC17607839"))),
        std_tag(StandardTag::Label(s("Label"))),
        std_tag(StandardTag::Mood(s("Calm"))),
        std_tag(StandardTag::Conductor(s("Conductor"))),
        std_tag(StandardTag::Remixer(s("Remixer"))),
        std_tag(StandardTag::Lyricist(s("Lyricist"))),
        std_tag(StandardTag::Work(s("Work"))),
        std_tag(StandardTag::Performer(s("Performer"))),
        std_tag(StandardTag::UrlArtist(s("https://example.org/artist"))),
        std_tag(StandardTag::TrackSubtitle(s("Sub"))),
        std_tag(StandardTag::DiscSubtitle(s("Disc Sub"))),
    ];
    tags.extend(new_tags());

    for version in [Id3v2Version::V4, Id3v2Version::V3] {
        let path = work_copy("mpeg-plain.mp3");
        write_file(&path, &tags, &[cover(b"\x89PNG-cover")], &opts(version)).unwrap();

        let bytes = fs::read(&path).unwrap();
        let (lens, v) = leading_tags(&bytes);
        assert_eq!(v, Some(if version == Id3v2Version::V3 { 3 } else { 4 }));
        assert_eq!(&bytes[lens[0]..], &audio()[..]);

        let read = read_back(&path);
        for tag in std_set(&tags) {
            assert!(read.std.contains(&tag), "{version:?}: missing {tag}");
        }
        assert_eq!(read.visuals.len(), 1);
        assert_eq!(decode_all(&path), decode_all(&fixture("mpeg-plain.mp3")));
        cleanup(&path);
    }
}

#[test]
fn version_policy() {
    let version_after = |name: &str, policy: Id3v2Version| {
        let path = work_copy(name);
        write_file(&path, &new_tags(), &[], &opts(policy)).unwrap();
        let version = leading_tags(&fs::read(&path).unwrap()).1;
        cleanup(&path);
        version
    };

    // Preserve keeps v2.3, and otherwise writes v2.4.
    assert_eq!(version_after("mpeg-v23.mp3", Id3v2Version::Preserve), Some(3));
    assert_eq!(version_after("mpeg-v24.mp3", Id3v2Version::Preserve), Some(4));
    assert_eq!(version_after("mpeg-plain.mp3", Id3v2Version::Preserve), Some(4));
    assert_eq!(version_after("mpeg-appended.mp3", Id3v2Version::Preserve), Some(4));
    assert_eq!(version_after("mpeg-stacked.mp3", Id3v2Version::Preserve), Some(3));

    // An explicit version converts the tag.
    assert_eq!(version_after("mpeg-v23.mp3", Id3v2Version::V4), Some(4));
    assert_eq!(version_after("mpeg-v24.mp3", Id3v2Version::V3), Some(3));

    // The unmanaged frames follow the conversion.
    let path = work_copy("mpeg-v23.mp3");
    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::V4)).unwrap();
    let read = read_back(&path);
    assert!(read.raw_keys.contains("PRIV"));
    assert!(read.raw_keys.contains("POPM"));
    assert!(read.raw_keys.contains("GEOB"));
    cleanup(&path);

    let path = work_copy("mpeg-v24.mp3");
    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::V3)).unwrap();
    let after = fs::read(&path).unwrap();
    let (lens, _) = leading_tags(&after);
    for needle in [&b"PRIV"[..], b"com.example.private24", b"POPM", b"CHAP", b"CTOC"] {
        assert!(
            contains(&after[..lens[0]], needle),
            "{:?} is missing",
            String::from_utf8_lossy(needle)
        );
    }
    // The chapter title was UTF-8, which does not exist in ID3v2.3: it is now UTF-16.
    let utf16: Vec<u8> = "Chapter One".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert!(contains(&after[..lens[0]], &utf16));
    // The 2.4 frame header sizes of the chapter's sub-frame became v2.3 sizes: the file reads.
    let read = read_back(&path);
    assert!(read.raw_keys.contains("PRIV"));
    cleanup(&path);
}

#[test]
fn empty_tags_remove_the_tags_but_keep_unmanaged_frames() {
    // A tag that only has managed frames goes away entirely.
    let path = work_copy("mpeg-plain.mp3");
    write_file(&path, &new_tags(), &[cover(b"\x89PNG-cover")], &opts(Id3v2Version::V4)).unwrap();
    assert_ne!(fs::read(&path).unwrap(), audio());

    write_file(&path, &[], &[], &opts(Id3v2Version::V4)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), audio());
    cleanup(&path);

    // Unmanaged frames are preserved, the ID3v1 tag goes.
    let path = work_copy("mpeg-v23.mp3");
    let before_len = fs::metadata(&path).unwrap().len();
    write_file(&path, &[], &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    assert!(!is_id3v1(&after));
    assert!(after.len() < before_len as usize - 128 + 1);
    let (lens, version) = leading_tags(&after);
    assert_eq!(version, Some(3));
    assert_eq!(lens.len(), 1);
    assert_eq!(&after[lens[0]..], &audio()[..]);

    let read = read_back(&path);
    assert!(read.std.is_empty(), "{:?}", read.std);
    assert!(read.visuals.is_empty());
    assert!(read.raw_keys.contains("PRIV"));
    cleanup(&path);
}

#[test]
fn untagged_file_stays_as_it_is_without_tags_and_gets_no_v1() {
    let path = work_copy("mpeg-plain.mp3");
    write_file(&path, &[], &[], &opts(Id3v2Version::Preserve)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), audio());

    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::Preserve)).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(!is_id3v1(&after));
    let (lens, _) = leading_tags(&after);
    assert_eq!(&after[lens[0]..], &audio()[..]);
    assert_eq!(sniff(&path).unwrap(), Container::MpegAudio);
    cleanup(&path);
}

#[test]
fn appended_tag_is_moved_to_the_front() {
    let path = work_copy("mpeg-appended.mp3");
    let before = fs::read(&path).unwrap();
    assert!(contains(&before, b"3DI"));
    assert!(is_id3v1(&before));

    let old = read_back(&path);
    assert!(old.std.contains(&format!("{:?}", StandardTag::TrackTitle(s("Appended Title")))));

    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    let (lens, version) = leading_tags(&after);
    assert_eq!(version, Some(4));
    assert_eq!(lens.len(), 1);

    // The footer is gone, the ID3v1 tag is rewritten, and the audio is where it was.
    assert!(!contains(&after, b"3DI"));
    assert!(is_id3v1(&after));
    let audio_end = after.len() - 128;
    assert_eq!(&after[lens[0]..audio_end], &audio()[..]);
    assert_eq!(&after[audio_end + 3..audio_end + 12], b"New Title");

    // The unmanaged frame of the appended tag moved to the new tag.
    assert!(contains(&after[..lens[0]], b"appended.priv"));
    let read = read_back(&path);
    assert!(!read.std.contains(&format!("{:?}", StandardTag::TrackTitle(s("Appended Title")))));
    assert!(read.std.contains(&format!("{:?}", StandardTag::TrackTitle(s("New Title")))));
    assert!(read.raw_keys.contains("PRIV"));
    cleanup(&path);
}

#[test]
fn stacked_tags_are_merged_into_one() {
    let path = work_copy("mpeg-stacked.mp3");
    let before = fs::read(&path).unwrap();
    assert_eq!(leading_tags(&before).0.len(), 2);

    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    let (lens, version) = leading_tags(&after);
    assert_eq!(lens.len(), 1);
    assert_eq!(version, Some(3));
    assert_eq!(&after[lens[0]..], &audio()[..]);

    // Both PRIV frames survived, the managed ones did not.
    assert!(contains(&after[..lens[0]], b"stack.a"));
    assert!(contains(&after[..lens[0]], b"stack.b"));
    assert!(!contains(&after[..lens[0]], b"Stacked A"));
    assert!(!contains(&after[..lens[0]], b"Stacked B"));
    cleanup(&path);
}

#[test]
fn ape_tag_is_not_touched() {
    let path = work_copy("mpeg-ape.mp3");
    let before = fs::read(&path).unwrap();
    let audio_len = audio().len();
    let ape = before[audio_len..before.len() - 128].to_vec();
    assert_eq!(&ape[..8], b"APETAGEX");

    write_file(&path, &new_tags(), &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    let (lens, _) = leading_tags(&after);
    assert_eq!(&after[lens[0]..lens[0] + audio_len], &audio()[..]);
    assert_eq!(&after[lens[0] + audio_len..after.len() - 128], &ape[..]);
    assert!(is_id3v1(&after));
    assert_eq!(&after[after.len() - 125..after.len() - 116], b"New Title");
    cleanup(&path);

    // With no tags, the ID3v1 tag is removed and the APE tag is still not touched.
    let path = work_copy("mpeg-ape.mp3");
    write_file(&path, &[], &[], &opts(Id3v2Version::Preserve)).unwrap();
    let after = fs::read(&path).unwrap();
    assert_eq!(after, before[..before.len() - 128].to_vec());
    cleanup(&path);
}

#[test]
fn id3v1_is_truncated_and_never_added() {
    let path = work_copy("mpeg-v23.mp3");
    let tags = vec![
        std_tag(StandardTag::TrackTitle(s("A title that is much longer than thirty characters"))),
        std_tag(StandardTag::Artist(s("Caf\u{e9} \u{20ac}"))),
        std_tag(StandardTag::RecordingDate(s("1999-01-01"))),
        std_tag(StandardTag::TrackNumber(7)),
        std_tag(StandardTag::Genre(s("Blues"))),
        std_tag(StandardTag::Comment(s("A comment that is longer than twenty-eight characters"))),
    ];
    write_file(&path, &tags, &[], &opts(Id3v2Version::Preserve)).unwrap();

    let after = fs::read(&path).unwrap();
    let v1 = &after[after.len() - 128..];
    assert_eq!(&v1[..3], b"TAG");
    assert_eq!(&v1[3..33], b"A title that is much longer th");
    assert_eq!(&v1[33..39], b"Caf\xe9 \x80");
    assert_eq!(&v1[93..97], b"1999");
    assert_eq!(&v1[97..125], b"A comment that is longer tha");
    assert_eq!(v1[125], 0);
    assert_eq!(v1[126], 7);
    assert_eq!(v1[127], 0);
    cleanup(&path);
}

#[test]
fn a_tag_that_does_not_fit_grows_the_padding_policy_applies() {
    let path = work_copy("mpeg-plain.mp3");

    // No padding: the tag is as large as its frames. 4 KiB padding: 4096 bytes more.
    write_file(
        &path,
        &new_tags(),
        &[],
        &WriteOptions { id3v2_version: Id3v2Version::V4, padding: 0 },
    )
    .unwrap();
    let exact = leading_tags(&fs::read(&path).unwrap()).0[0];

    let path2 = work_copy("mpeg-plain.mp3");
    write_file(
        &path2,
        &new_tags(),
        &[],
        &WriteOptions { id3v2_version: Id3v2Version::V4, padding: 4096 },
    )
    .unwrap();
    let padded = fs::read(&path2).unwrap();
    assert_eq!(leading_tags(&padded).0[0], exact + 4096);

    // Rewriting with a tag that fits in the padding keeps the size, and the audio in place.
    let smaller = vec![std_tag(StandardTag::TrackTitle(s("Short")))];
    write_file(
        &path2,
        &smaller,
        &[],
        &WriteOptions { id3v2_version: Id3v2Version::V4, padding: 4096 },
    )
    .unwrap();
    let again = fs::read(&path2).unwrap();
    assert_eq!(leading_tags(&again).0[0], exact + 4096);
    assert_eq!(&again[exact + 4096..], &audio()[..]);

    cleanup(&path);
    cleanup(&path2);
}

#[test]
fn damaged_files_are_rejected_without_changes() {
    let path = work_copy("mpeg-v23.mp3");
    let bytes = fs::read(&path).unwrap();
    // Truncate in the middle of the ID3v2 tag.
    fs::write(&path, &bytes[..100]).unwrap();

    let err = write_file(&path, &new_tags(), &[], &opts(Id3v2Version::Preserve)).unwrap_err();
    assert!(matches!(err, Error::Malformed(_)), "{err:?}");
    assert_eq!(fs::read(&path).unwrap(), &bytes[..100]);

    // No temporary files are left behind.
    let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
    assert_eq!(leftovers, 1);
    cleanup(&path);
}

#[test]
fn comment_and_lyrics_languages_and_descriptions_are_kept() {
    let path = work_copy("mpeg-plain.mp3");

    let comment = Tag::new_std(
        RawTag::new_with_sub_fields(
            "COMM",
            "Un commentaire",
            vec![
                RawTagSubField::new("LANGUAGE", "fra"),
                RawTagSubField::new("SHORT_DESCRIPTION", "desc"),
            ]
            .into_boxed_slice(),
        ),
        StandardTag::Comment(s("Un commentaire")),
    );

    write_file(&path, &[comment], &[], &opts(Id3v2Version::V4)).unwrap();

    let mut format = open(&path);
    let rev = format.metadata().current().unwrap().clone();
    let tag =
        rev.media.tags.iter().find(|t| matches!(t.std, Some(StandardTag::Comment(_)))).unwrap();
    let fields = tag.raw.sub_fields.as_ref().unwrap();
    assert!(fields.iter().any(|f| f.field == "LANGUAGE" && f.value.to_string() == "fra"));
    assert!(fields.iter().any(|f| f.field == "SHORT_DESCRIPTION" && f.value.to_string() == "desc"));
    cleanup(&path);
}

#[test]
fn audio_packets_are_identical_after_a_write() {
    for name in
        ["mpeg-v23.mp3", "mpeg-v24.mp3", "mpeg-appended.mp3", "mpeg-stacked.mp3", "mpeg-ape.mp3"]
    {
        let path = work_copy(name);
        let before = audio_packets(&path);
        assert!(!before.is_empty(), "{name}");

        write_file(&path, &new_tags(), &[cover(b"\x89PNG-cover")], &opts(Id3v2Version::Preserve))
            .unwrap();
        assert_eq!(audio_packets(&path), before, "{name}");
        cleanup(&path);
    }
}
