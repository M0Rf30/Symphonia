#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the FLAC (`flac_id3`, `remove_empty_flac_seektable`) and WAVE (`riff_info`,
//! `riff_info_encoding`) write options.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawTag, StandardTag, Tag};
use symphonia_tagging::{FlacId3, RiffInfoEncoding, RiffInfoPolicy, WriteOptions, write_file};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Copies a fixture into a fresh directory and returns its path.
fn fixture(test: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("symphonia-tagging-{}-{test}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(name);
    fs::copy(Path::new(FIXTURES).join(name), &dst).unwrap();
    dst
}

/// A path for a synthesized file.
fn scratch(test: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("symphonia-tagging-{}-{test}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

fn title(v: &str) -> Tag {
    Tag::new_std(RawTag::new("TITLE", v), StandardTag::TrackTitle(s(v)))
}

/// Reads all the standard tags through the default probe.
fn read_std_tags(path: &Path) -> Vec<StandardTag> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap_or_else(|e| panic!("{} does not probe: {e}", path.display()));

    let mut metadata = format.metadata();
    let mut tags = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            tags.extend(revision.media.tags.iter().filter_map(|t| t.std.clone()));
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    tags
}

// --- FLAC ------------------------------------------------------------------------------------

/// Builds an ID3v2.3 tag with a TIT2 frame.
fn id3v2_tag(text: &str) -> Vec<u8> {
    let mut frame = b"TIT2".to_vec();
    frame.extend_from_slice(&(1 + text.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0]);
    frame.extend_from_slice(text.as_bytes());

    let size = frame.len() as u32;

    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend_from_slice(&[
        (size >> 21) as u8 & 0x7f,
        (size >> 14) as u8 & 0x7f,
        (size >> 7) as u8 & 0x7f,
        size as u8 & 0x7f,
    ]);
    tag.extend_from_slice(&frame);
    tag
}

/// A metadata block of a FLAC file.
struct Block {
    kind: u8,
    is_last: bool,
    body: Vec<u8>,
}

/// Parses a FLAC file that starts at `fLaC`: the metadata blocks and the audio.
fn parse_flac(bytes: &[u8]) -> (Vec<Block>, Vec<u8>) {
    assert_eq!(&bytes[..4], b"fLaC");

    let mut pos = 4;
    let mut blocks = Vec::new();

    loop {
        let head = &bytes[pos..pos + 4];
        let is_last = head[0] & 0x80 != 0;
        let len = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;

        blocks.push(Block {
            kind: head[0] & 0x7f,
            is_last,
            body: bytes[pos + 4..pos + 4 + len].to_vec(),
        });
        pos += 4 + len;

        if is_last {
            break;
        }
    }

    (blocks, bytes[pos..].to_vec())
}

fn block_bytes(kind: u8, is_last: bool, body: &[u8]) -> Vec<u8> {
    let len = body.len() as u32;
    let mut out = vec![kind | if is_last { 0x80 } else { 0 }];
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

/// Builds a FLAC file out of the `STREAMINFO` and the audio of the plain fixture, with `extra`
/// blocks between `STREAMINFO` and an empty comment block, and `prefix` before `fLaC`.
fn synth_flac(prefix: &[u8], extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let original = fs::read(Path::new(FIXTURES).join("vorbis-plain.flac")).unwrap();
    let (blocks, audio) = parse_flac(&original);
    assert_eq!(blocks[0].kind, 0);

    let mut out = prefix.to_vec();
    out.extend_from_slice(b"fLaC");
    out.extend(block_bytes(0, false, &blocks[0].body));

    for (kind, body) in extra {
        out.extend(block_bytes(*kind, false, body));
    }

    // An empty comment: empty vendor, no comments.
    out.extend(block_bytes(4, true, &[0; 8]));
    out.extend_from_slice(&audio);
    out
}

/// Writes tags to a FLAC file with the leading `prefix` with both policies.
fn check_flac_id3(test: &str, prefix: Vec<u8>) {
    let original = synth_flac(&prefix, &[]);
    let (_, audio) = parse_flac(&original[prefix.len()..]);

    for (policy, kept) in [(FlacId3::Remove, false), (FlacId3::Keep, true)] {
        let path = scratch(test, &format!("{policy:?}.flac"));
        fs::write(&path, &original).unwrap();

        let opts = WriteOptions { flac_id3: policy, ..Default::default() };
        write_file(&path, &[title("Title")], &[], &opts).unwrap();

        let out = fs::read(&path).unwrap();

        let flac_at = if kept {
            assert_eq!(&out[..prefix.len()], &prefix[..], "the tags are kept byte for byte");
            prefix.len()
        }
        else {
            assert_eq!(&out[..4], b"fLaC", "the tags are dropped");
            0
        };

        let (blocks, out_audio) = parse_flac(&out[flac_at..]);
        assert_eq!(blocks[0].kind, 0, "STREAMINFO is first");
        assert!(blocks.iter().filter(|b| b.is_last).count() == 1 && blocks.last().unwrap().is_last);
        assert_eq!(out_audio, audio, "the audio frames are unchanged");

        // The title is readable.
        assert!(read_std_tags(&path).contains(&StandardTag::TrackTitle(s("Title"))));
    }
}

#[test]
fn flac_id3_single_tag_is_dropped_or_kept() {
    check_flac_id3("flac_id3_single", id3v2_tag("only tag"));
}

#[test]
fn flac_id3_stacked_tags_are_dropped_or_kept() {
    let mut prefix = id3v2_tag("first tag");
    prefix.extend_from_slice(&id3v2_tag("second, stacked tag"));
    check_flac_id3("flac_id3_stacked", prefix);
}

#[test]
fn flac_id3_keep_leaves_the_audio_where_it_is_when_the_chain_fits() {
    let tag = id3v2_tag("keep me");

    let mut original = tag.clone();
    original.extend_from_slice(&fs::read(Path::new(FIXTURES).join("vorbis-plain.flac")).unwrap());

    let path = scratch("flac_id3_offset", "kept.flac");
    fs::write(&path, &original).unwrap();

    let opts = WriteOptions { flac_id3: FlacId3::Keep, ..Default::default() };
    write_file(&path, &[title("T")], &[], &opts).unwrap();

    let out = fs::read(&path).unwrap();

    // The old chain (with its padding) had room for the new one, so the file does not change size.
    assert_eq!(out.len(), original.len());
    assert_eq!(&out[..tag.len()], &tag[..]);
}

#[test]
fn flac_empty_seektable_is_removed_only_when_asked_and_empty() {
    let one_point = {
        let mut point = Vec::new();
        point.extend_from_slice(&0u64.to_be_bytes());
        point.extend_from_slice(&0u64.to_be_bytes());
        point.extend_from_slice(&4096u16.to_be_bytes());
        point
    };

    let kinds = |bytes: &[u8]| -> Vec<u8> { parse_flac(bytes).0.iter().map(|b| b.kind).collect() };

    // Empty table.
    let empty = synth_flac(&[], &[(3, Vec::new())]);

    let path = scratch("flac_seektable", "empty-keep.flac");
    fs::write(&path, &empty).unwrap();
    write_file(&path, &[title("T")], &[], &WriteOptions::default()).unwrap();
    assert!(kinds(&fs::read(&path).unwrap()).contains(&3), "kept by default");

    let path = scratch("flac_seektable", "empty-remove.flac");
    fs::write(&path, &empty).unwrap();
    let opts = WriteOptions { remove_empty_flac_seektable: true, ..Default::default() };
    write_file(&path, &[title("T")], &[], &opts).unwrap();
    let out = fs::read(&path).unwrap();
    assert!(!kinds(&out).contains(&3), "removed when empty");
    assert_eq!(kinds(&out)[0], 0);
    assert_eq!(parse_flac(&out).1, parse_flac(&empty).1, "the audio is unchanged");

    // Non-empty table.
    let full = synth_flac(&[], &[(3, one_point.clone())]);

    let path = scratch("flac_seektable", "full-remove.flac");
    fs::write(&path, &full).unwrap();
    write_file(&path, &[title("T")], &[], &opts).unwrap();
    let (blocks, _) = parse_flac(&fs::read(&path).unwrap());
    let table = blocks.iter().find(|b| b.kind == 3).expect("a non-empty table is kept");
    assert_eq!(table.body, one_point);
}

// --- WAVE ------------------------------------------------------------------------------------

/// A top-level chunk of a WAVE file, as its id, payload, and complete bytes.
struct Chunk {
    id: [u8; 4],
    payload: Vec<u8>,
    bytes: Vec<u8>,
}

impl Chunk {
    fn is_info(&self) -> bool {
        &self.id == b"LIST" && self.payload.starts_with(b"INFO")
    }
}

fn chunks(bytes: &[u8]) -> Vec<Chunk> {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");

    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff_size + 8, bytes.len(), "the RIFF size is right");

    let mut pos = 12;
    let mut out = Vec::new();

    while pos + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let end = pos + 8 + size + (size & 1);

        out.push(Chunk {
            id: bytes[pos..pos + 4].try_into().unwrap(),
            payload: bytes[pos + 8..pos + 8 + size].to_vec(),
            bytes: bytes[pos..end.min(bytes.len())].to_vec(),
        });
        pos = end;
    }

    out
}

/// The chunks that are neither an INFO list nor an `id3 ` chunk.
fn other_chunks(bytes: &[u8]) -> Vec<Vec<u8>> {
    chunks(bytes)
        .into_iter()
        .filter(|c| !c.is_info() && &c.id != b"id3 ")
        .map(|c| c.bytes)
        .collect()
}

/// The payload of the INFO sub-chunk `id` of the first INFO list.
fn info_sub_chunk(bytes: &[u8], id: &[u8; 4]) -> Option<Vec<u8>> {
    let list = chunks(bytes).into_iter().find(Chunk::is_info)?;
    let mut buf = &list.payload[4..];

    while buf.len() >= 8 {
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;

        if &buf[..4] == id {
            return Some(buf[8..8 + len].to_vec());
        }

        buf = &buf[(8 + len + (len & 1)).min(buf.len())..];
    }

    None
}

#[test]
fn wave_info_write_remove_and_keep() {
    let original = fs::read(Path::new(FIXTURES).join("riffaiffdsf-lavf-info.wav")).unwrap();
    let original_info: Vec<Vec<u8>> =
        chunks(&original).into_iter().filter(Chunk::is_info).map(|c| c.bytes).collect();
    assert!(!original_info.is_empty(), "the fixture has an INFO list");

    for policy in [RiffInfoPolicy::Write, RiffInfoPolicy::Remove, RiffInfoPolicy::Keep] {
        let path = fixture(&format!("wave_info_{policy:?}"), "riffaiffdsf-lavf-info.wav");

        let opts = WriteOptions { riff_info: policy, ..Default::default() };
        write_file(&path, &[title("New Title")], &[], &opts).unwrap();

        let out = fs::read(&path).unwrap();
        let info: Vec<Vec<u8>> =
            chunks(&out).into_iter().filter(Chunk::is_info).map(|c| c.bytes).collect();

        // `fmt `, `data` and every other chunk are unchanged, whatever the policy.
        assert_eq!(other_chunks(&out), other_chunks(&original));

        match policy {
            RiffInfoPolicy::Write => {
                assert_eq!(info.len(), 1);
                assert_eq!(info_sub_chunk(&out, b"INAM").unwrap(), b"New Title\0");
            }
            RiffInfoPolicy::Remove => {
                assert!(info.is_empty(), "every INFO list is removed");
            }
            RiffInfoPolicy::Keep => {
                assert_eq!(info, original_info, "the INFO lists are left byte for byte");
            }
        }

        // The `id3 ` chunk is independent of the INFO policy: the title is always readable.
        assert!(read_std_tags(&path).contains(&StandardTag::TrackTitle(s("New Title"))));
    }
}

#[test]
fn wave_info_windows_1252_and_utf8_text() {
    let text = "Café €";

    let utf8 = fixture("wave_info_utf8", "riffaiffdsf-ffmpeg.wav");
    write_file(&utf8, &[title(text)], &[], &WriteOptions::default()).unwrap();
    assert_eq!(info_sub_chunk(&fs::read(&utf8).unwrap(), b"INAM").unwrap(), "Café €\0".as_bytes());

    let opts = WriteOptions {
        riff_info_encoding: RiffInfoEncoding::Windows1252,
        // The ID3v2 tag would also carry the title: drop it from the picture, only INFO counts.
        ..Default::default()
    };

    let cp1252 = fixture("wave_info_1252", "riffaiffdsf-ffmpeg.wav");
    write_file(&cp1252, &[title(text)], &[], &opts).unwrap();

    let out = fs::read(&cp1252).unwrap();
    assert_eq!(info_sub_chunk(&out, b"INAM").unwrap(), b"Caf\xe9 \x80\0");

    // The reader decodes the bytes back to the same text.
    let list = chunks(&out).into_iter().find(Chunk::is_info).unwrap();
    assert_eq!(list.payload.len() % 2, 0);

    let read = read_std_tags(&cp1252);
    assert!(read.contains(&StandardTag::TrackTitle(s(text))));

    // The INFO text alone reads back as the title too: remove the id3 chunk from the picture by
    // writing without the ID3-carrying title... the INFO list must decode on its own.
    let mut info_only = Vec::new();
    info_only.extend_from_slice(&out[..12]);
    for chunk in chunks(&out).iter().filter(|c| &c.id != b"id3 ") {
        info_only.extend_from_slice(&chunk.bytes);
    }
    let size = (info_only.len() - 8) as u32;
    info_only[4..8].copy_from_slice(&size.to_le_bytes());

    let only = scratch("wave_info_1252_only", "info-only.wav");
    fs::write(&only, &info_only).unwrap();
    assert!(read_std_tags(&only).contains(&StandardTag::TrackTitle(s(text))));
}

#[test]
fn wave_info_windows_1252_encodes_the_special_block_and_replaces_the_rest() {
    let opts =
        WriteOptions { riff_info_encoding: RiffInfoEncoding::Windows1252, ..Default::default() };

    let path = fixture("wave_info_1252_chars", "riffaiffdsf-ffmpeg.wav");

    let text = "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ ÿ a日b";
    write_file(&path, &[title(text)], &[], &opts).unwrap();

    let out = fs::read(&path).unwrap();
    let expected: Vec<u8> = [
        0x80, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8e, 0x91, 0x92,
        0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9e, 0x9f, b' ', 0xff, b' ',
        b'a', b'?', b'b', 0,
    ]
    .to_vec();

    assert_eq!(info_sub_chunk(&out, b"INAM").unwrap(), expected, "unrepresentable becomes ?");
}
