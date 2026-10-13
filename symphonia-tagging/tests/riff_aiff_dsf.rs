#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip tests for the tags of RIFF WAVE, AIFF and DSF files.

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
use symphonia::core::meta::{MetadataOptions, RawTag, StandardTag, StandardVisualKey, Tag, Visual};
use symphonia_tagging::{Container, Error, Id3v2Version, WriteOptions, sniff, write_file};

// ---------------------------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Writes `bytes` to a file in a private temporary directory.
fn work_file(name: &str, bytes: &[u8]) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "symphonia-tagging-riff-aiff-dsf-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// Removes the temporary directory of a work file.
fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

fn std_tag(raw: &str, std: StandardTag) -> Tag {
    Tag::new_std(RawTag::new(raw, "x"), std)
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

fn opts() -> WriteOptions {
    WriteOptions { id3v2_version: Id3v2Version::V4, padding: 0, ..Default::default() }
}

/// The tags the standard tags of `tags` should read back as.
fn sample_tags() -> Vec<Tag> {
    vec![
        std_tag("TIT2", StandardTag::TrackTitle(s("New Title"))),
        std_tag("TPE1", StandardTag::Artist(s("New Artist"))),
        std_tag("TALB", StandardTag::Album(s("New Album"))),
        std_tag("COMM", StandardTag::Comment(s("New comment"))),
        std_tag("TRCK", StandardTag::TrackNumber(5)),
        Tag::new(RawTag::new("MYKEY", "free-form value")),
    ]
}

/// What `sample_tags` read back as (one entry per distinct standard tag).
fn sample_std() -> BTreeSet<String> {
    sample_tags().iter().filter_map(|t| t.std.as_ref()).map(|t| format!("{t:?}")).collect()
}

struct ReadBack {
    /// Distinct standard tags, over all metadata revisions (INFO/text chunks and ID3).
    std: BTreeSet<String>,
    visuals: Vec<(Option<StandardVisualKey>, Vec<u8>)>,
}

fn open(path: &Path, ext: &str) -> Box<dyn symphonia::core::formats::FormatReader + 'static> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);

    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe")
}

/// Reads the tags and visuals the fork's own probe finds.
fn read_back(path: &Path, ext: &str) -> ReadBack {
    let mut format = open(path, ext);

    let mut std = BTreeSet::new();
    let mut visuals: Vec<(Option<StandardVisualKey>, Vec<u8>)> = Vec::new();

    loop {
        if let Some(rev) = format.metadata().current() {
            for tag in &rev.media.tags {
                if let Some(t) = &tag.std {
                    std.insert(format!("{t:?}"));
                }
            }
            for v in &rev.media.visuals {
                let key = (v.usage, v.data.to_vec());
                if !visuals.contains(&key) {
                    visuals.push(key);
                }
            }
        }

        if format.metadata().is_latest() {
            break;
        }
        format.metadata().pop();
    }

    ReadBack { std, visuals }
}

/// The bytes of all audio packets.
fn audio_bytes(path: &Path, ext: &str) -> Vec<u8> {
    let mut format = open(path, ext);
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
fn decode_all(path: &Path, ext: &str) -> usize {
    let mut format = open(path, ext);

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

/// A chunk of a parsed file: id and the complete bytes (header, payload, pad).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawChunk {
    id: [u8; 4],
    payload: Vec<u8>,
    bytes: Vec<u8>,
}

/// Splits a RIFF/FORM file into its chunks. Returns the chunks and the bytes after the last one.
/// Also checks the size in the file header.
fn chunks_of(file: &[u8], big_endian: bool) -> (Vec<RawChunk>, Vec<u8>) {
    let rd = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        (if big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }) as usize
    };

    let mut chunks = Vec::new();
    let mut pos = 12;

    while pos + 8 <= file.len() {
        let id = [file[pos], file[pos + 1], file[pos + 2], file[pos + 3]];
        if !id.iter().all(|b| (0x20..0x7f).contains(b)) {
            break;
        }
        let size = rd(&file[pos + 4..pos + 8]);
        let end = pos + 8 + size;
        assert!(end <= file.len(), "chunk past the end of the file");
        let padded = (end + (size & 1)).min(file.len());
        chunks.push(RawChunk {
            id,
            payload: file[pos + 8..end].to_vec(),
            bytes: file[pos..padded].to_vec(),
        });
        pos = padded;
    }

    (chunks, file[pos..].to_vec())
}

fn find<'a>(chunks: &'a [RawChunk], id: &[u8; 4]) -> Vec<&'a RawChunk> {
    chunks.iter().filter(|c| &c.id == id).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------------------------
// File builders.
// ---------------------------------------------------------------------------------------------

fn chunk(id: &[u8; 4], payload: &[u8], big_endian: bool) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = id.to_vec();
    out.extend_from_slice(&if big_endian { len.to_be_bytes() } else { len.to_le_bytes() });
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn riff_file(magic: &[u8; 4], form: &[u8; 4], big_endian: bool, chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let size = (4 + body.len()) as u32;
    let mut out = magic.to_vec();
    out.extend_from_slice(&if big_endian { size.to_be_bytes() } else { size.to_le_bytes() });
    out.extend_from_slice(form);
    out.extend_from_slice(&body);
    out
}

fn wav_fmt() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes()); // PCM
    f.extend_from_slice(&1u16.to_le_bytes()); // mono
    f.extend_from_slice(&8000u32.to_le_bytes());
    f.extend_from_slice(&16000u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    chunk(b"fmt ", &f, false)
}

/// 200 frames of a deterministic ramp.
fn wav_samples() -> Vec<u8> {
    (0..200i16).flat_map(|i| (i * 100 - 10_000).to_le_bytes()).collect()
}

fn syncsafe(n: usize) -> [u8; 4] {
    symphonia_metadata::id3v2::write::encode_synchsafe(u32::try_from(n).unwrap())
}

/// An ID3v2.4 tag.
fn id3_tag(frames: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, data) in frames {
        body.extend_from_slice(*id);
        body.extend_from_slice(&syncsafe(data.len()));
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(data);
    }
    let mut out = b"ID3\x04\x00\x00".to_vec();
    out.extend_from_slice(&syncsafe(body.len()));
    out.extend_from_slice(&body);
    out
}

fn text_frame(text: &str) -> Vec<u8> {
    let mut v = vec![3u8];
    v.extend_from_slice(text.as_bytes());
    v
}

/// An ID3v2.4 tag with an old title, and the frames the tag model does not represent.
fn old_id3_with_priv_popm() -> Vec<u8> {
    let mut priv_frame = b"com.example.owner\0".to_vec();
    priv_frame.extend_from_slice(&[1, 2, 3, 4, 5]);

    let mut popm = b"me@example.com\0".to_vec();
    popm.push(196);
    popm.extend_from_slice(&7u32.to_be_bytes());

    id3_tag(&[(b"TIT2", text_frame("Old Title")), (b"PRIV", priv_frame), (b"POPM", popm)])
}

fn info_list(subchunks: &[(&[u8; 4], &str)]) -> Vec<u8> {
    let mut payload = b"INFO".to_vec();
    for (id, text) in subchunks {
        let mut v = text.as_bytes().to_vec();
        v.push(0);
        payload.extend_from_slice(&chunk(id, &v, false));
    }
    chunk(b"LIST", &payload, false)
}

/// A WAVE file with INFO tags (managed and not), an ID3 chunk with unmanaged frames, and other
/// chunks, including one of an odd size.
fn rich_wav(id3_id: &[u8; 4]) -> Vec<u8> {
    riff_file(
        b"RIFF",
        b"WAVE",
        false,
        &[
            wav_fmt(),
            chunk(b"fact", &200u32.to_le_bytes(), false),
            chunk(b"bext", b"broadcast ext data!", false),
            chunk(b"xtra", b"odd!!", false),
            info_list(&[(b"INAM", "Old Info Title"), (b"ISBJ", "kept subject")]),
            chunk(b"data", &wav_samples(), false),
            chunk(id3_id, &old_id3_with_priv_popm(), false),
            chunk(b"LIST", b"adtlsome cue labels..", false),
        ],
    )
}

fn aiff_comm() -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&1i16.to_be_bytes()); // mono
    c.extend_from_slice(&200u32.to_be_bytes()); // frames
    c.extend_from_slice(&16i16.to_be_bytes());
    // 8000 Hz as an 80-bit extended float.
    c.extend_from_slice(&[0x40, 0x0b, 0xfa, 0x00, 0, 0, 0, 0, 0, 0]);
    chunk(b"COMM", &c, true)
}

fn aiff_ssnd() -> Vec<u8> {
    let mut c = vec![0u8; 8]; // offset, block size
    c.extend((0..200i16).flat_map(|i| (i * 100 - 10_000).to_be_bytes()));
    chunk(b"SSND", &c, true)
}

fn rich_aiff(id3_id: &[u8; 4]) -> Vec<u8> {
    riff_file(
        b"FORM",
        b"AIFF",
        true,
        &[
            aiff_comm(),
            chunk(b"NAME", b"Old Name", true),
            chunk(b"ANNO", b"old note", true),
            chunk(b"APPL", b"testsome app data", true),
            chunk(b"XYZW", b"odd", true),
            aiff_ssnd(),
            chunk(id3_id, &old_id3_with_priv_popm(), true),
        ],
    )
}

/// A DSF file with 2 channels and a single 256 byte block per channel.
fn dsf_file(data: &[u8], id3: Option<&[u8]>) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u32.to_le_bytes()); // version
    fmt.extend_from_slice(&0u32.to_le_bytes()); // format id (raw DSD)
    fmt.extend_from_slice(&2u32.to_le_bytes()); // channel type: stereo
    fmt.extend_from_slice(&2u32.to_le_bytes()); // channel count
    fmt.extend_from_slice(&2_822_400u32.to_le_bytes());
    fmt.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
    fmt.extend_from_slice(&2048u64.to_le_bytes()); // samples per channel
    fmt.extend_from_slice(&256u32.to_le_bytes()); // block size per channel
    fmt.extend_from_slice(&0u32.to_le_bytes());

    let data_chunk_len = 12 + data.len() as u64;
    let data_end = 28 + 52 + data_chunk_len;
    let total = data_end + id3.map_or(0, |t| t.len() as u64);

    let mut out = b"DSD ".to_vec();
    out.extend_from_slice(&28u64.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&if id3.is_some() { data_end } else { 0 }.to_le_bytes());
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_chunk_len.to_le_bytes());
    out.extend_from_slice(data);
    if let Some(t) = id3 {
        out.extend_from_slice(t);
    }
    out
}

fn dsf_data() -> Vec<u8> {
    (0..512u32).map(|i| (i * 7 % 251) as u8).collect()
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

// ---------------------------------------------------------------------------------------------
// WAVE.
// ---------------------------------------------------------------------------------------------

#[test]
fn wav_rewrites_tags_and_preserves_everything_else() {
    for old_id3_id in [b"id3 ", b"ID3 "] {
        let original = rich_wav(old_id3_id);
        let path = work_file("rich.wav", &original);

        let before_audio = audio_bytes(&path, "wav");
        let frames = decode_all(&path, "wav");

        let visuals = [cover(b"\x89PNG-not-really-a-png")];
        write_file(&path, &sample_tags(), &visuals, &opts()).unwrap();

        let out = fs::read(&path).unwrap();
        let (old_chunks, _) = chunks_of(&original, false);
        let (new_chunks, junk) = chunks_of(&out, false);

        assert!(junk.is_empty());
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert_eq!(&out[8..12], b"WAVE");

        // Chunks that are not tags are byte-identical and in order.
        let is_tag = |c: &RawChunk| {
            c.id == *b"id3 " || c.id == *b"ID3 " || (c.id == *b"LIST" && &c.payload[..4] == b"INFO")
        };
        let kept = |chunks: &[RawChunk]| -> Vec<RawChunk> {
            chunks.iter().filter(|c| !is_tag(c)).cloned().collect()
        };
        assert_eq!(kept(&old_chunks), kept(&new_chunks));

        // One lower-case ID3 chunk (in place of the old one), which kept the unmanaged frames.
        assert!(find(&new_chunks, b"ID3 ").is_empty());
        let id3 = find(&new_chunks, b"id3 ");
        assert_eq!(id3.len(), 1);
        assert!(contains(&id3[0].payload, b"PRIV"));
        assert!(contains(&id3[0].payload, b"com.example.owner"));
        assert!(contains(&id3[0].payload, b"POPM"));
        assert!(contains(&id3[0].payload, b"me@example.com"));
        assert!(!contains(&id3[0].payload, b"Old Title"));
        assert!(contains(&id3[0].payload, b"New Title"));
        let id3_pos = new_chunks.iter().position(|c| c.id == *b"id3 ").unwrap();
        let old_id3_pos = old_chunks.iter().position(|c| c.id == *old_id3_id).unwrap();
        assert_eq!(id3_pos, old_id3_pos);

        // One INFO list, in place, with the new tags and the unmanaged sub-chunk.
        let infos: Vec<_> =
            new_chunks.iter().filter(|c| c.id == *b"LIST" && &c.payload[..4] == b"INFO").collect();
        assert_eq!(infos.len(), 1);
        let info = &infos[0].payload;
        assert!(contains(info, b"INAM"));
        assert!(contains(info, b"New Title\0"));
        assert!(!contains(info, b"Old Info Title"));
        assert!(contains(info, b"ISBJ"));
        assert!(contains(info, b"kept subject\0"));

        // Reads back the same tags and visuals.
        let back = read_back(&path, "wav");
        let mut expected = sample_std();
        // The old POPM frame is preserved, and read as a rating and a play counter.
        let extra: Vec<_> = back
            .std
            .iter()
            .filter(|t| t.starts_with("Rating") || t.starts_with("PlayCounter"))
            .cloned()
            .collect();
        expected.extend(extra);
        assert_eq!(back.std, expected);
        assert_eq!(
            back.visuals,
            vec![(Some(StandardVisualKey::FrontCover), visuals[0].data.to_vec())]
        );

        // Audio is untouched.
        assert_eq!(audio_bytes(&path, "wav"), before_audio);
        assert_eq!(decode_all(&path, "wav"), frames);

        cleanup(&path);
    }
}

#[test]
fn wav_without_tags_gets_chunks_after_the_data() {
    let original =
        riff_file(b"RIFF", b"WAVE", false, &[wav_fmt(), chunk(b"data", &wav_samples(), false)]);
    let path = work_file("bare.wav", &original);
    let before_audio = audio_bytes(&path, "wav");

    write_file(&path, &sample_tags(), &[cover(b"cover")], &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let (chunks, junk) = chunks_of(&out, false);
    assert!(junk.is_empty());
    let ids: Vec<_> = chunks.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![*b"fmt ", *b"data", *b"LIST", *b"id3 "]);
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);

    let (old_chunks, _) = chunks_of(&original, false);
    assert_eq!(&chunks[..2], &old_chunks[..]);

    let back = read_back(&path, "wav");
    assert_eq!(back.std, sample_std());
    assert_eq!(back.visuals, vec![(Some(StandardVisualKey::FrontCover), b"cover".to_vec())]);
    assert_eq!(audio_bytes(&path, "wav"), before_audio);

    // Writing the same tags again changes nothing.
    write_file(&path, &sample_tags(), &[cover(b"cover")], &opts()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), out);

    cleanup(&path);
}

#[test]
fn wav_empty_tags_remove_the_chunks() {
    // The old ID3 tag has no unmanaged frames, and the INFO list only managed sub-chunks.
    let original = riff_file(
        b"RIFF",
        b"WAVE",
        false,
        &[
            wav_fmt(),
            info_list(&[(b"INAM", "Old"), (b"IART", "Older")]),
            chunk(b"data", &wav_samples(), false),
            chunk(b"id3 ", &id3_tag(&[(b"TIT2", text_frame("Old"))]), false),
        ],
    );
    let path = work_file("remove.wav", &original);

    write_file(&path, &[], &[], &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let (chunks, junk) = chunks_of(&out, false);
    assert!(junk.is_empty());
    let ids: Vec<_> = chunks.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![*b"fmt ", *b"data"]);
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
    assert!(read_back(&path, "wav").std.is_empty());
    assert_eq!(decode_all(&path, "wav"), 200);

    cleanup(&path);
}

#[test]
fn wav_empty_tags_keep_unmanaged_data() {
    let original = rich_wav(b"id3 ");
    let path = work_file("keep.wav", &original);

    write_file(&path, &[], &[], &opts()).unwrap();

    let (chunks, _) = chunks_of(&fs::read(&path).unwrap(), false);

    // The unmanaged INFO sub-chunk and the PRIV/POPM frames remain, the old title does not.
    let info = chunks.iter().find(|c| c.id == *b"LIST" && &c.payload[..4] == b"INFO").unwrap();
    assert!(contains(&info.payload, b"ISBJ"));
    assert!(!contains(&info.payload, b"INAM"));
    let id3 = find(&chunks, b"id3 ");
    assert_eq!(id3.len(), 1);
    assert!(contains(&id3[0].payload, b"PRIV"));
    assert!(contains(&id3[0].payload, b"POPM"));
    assert!(!contains(&id3[0].payload, b"Old Title"));

    cleanup(&path);
}

#[test]
fn wav_trailing_junk_and_a_missing_final_pad_are_handled() {
    // The last chunk has an odd size and no pad byte, followed by nothing.
    let mut original = riff_file(
        b"RIFF",
        b"WAVE",
        false,
        &[wav_fmt(), chunk(b"data", &wav_samples(), false), chunk(b"xtra", b"odd", false)],
    );
    original.pop();
    let path = work_file("nopad.wav", &original);
    write_file(&path, &sample_tags(), &[], &opts()).unwrap();
    let out = fs::read(&path).unwrap();
    let (chunks, junk) = chunks_of(&out, false);
    assert!(junk.is_empty());
    let ids: Vec<_> = chunks.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![*b"fmt ", *b"data", *b"xtra", *b"LIST", *b"id3 "]);
    assert_eq!(chunks[2].bytes, chunk(b"xtra", b"odd", false));
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
    cleanup(&path);

    // Bytes that are not a chunk follow the last chunk, and stay after the new chunks.
    let mut original = rich_wav(b"id3 ");
    original.extend_from_slice(b"\0\x01junk");
    let path = work_file("junk.wav", &original);
    write_file(&path, &sample_tags(), &[], &opts()).unwrap();
    let out = fs::read(&path).unwrap();
    let (chunks, junk) = chunks_of(&out, false);
    assert_eq!(junk, b"\0\x01junk");
    assert_eq!(
        u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
        out.len() - 8 - junk.len()
    );
    assert_eq!(find(&chunks, b"id3 ").len(), 1);
    assert!(read_back(&path, "wav").std.contains("TrackTitle(\"New Title\")"));
    cleanup(&path);
}

#[test]
fn wav_rf64_is_unsupported_and_left_alone() {
    let mut ds64 = Vec::new();
    ds64.extend_from_slice(&0u64.to_le_bytes()); // riff size
    ds64.extend_from_slice(&(wav_samples().len() as u64).to_le_bytes()); // data size
    ds64.extend_from_slice(&200u64.to_le_bytes()); // sample count
    ds64.extend_from_slice(&0u32.to_le_bytes()); // table length

    let mut data = b"data".to_vec();
    data.extend_from_slice(&u32::MAX.to_le_bytes());
    data.extend_from_slice(&wav_samples());

    let mut original =
        riff_file(b"RF64", b"WAVE", false, &[chunk(b"ds64", &ds64, false), wav_fmt()]);
    original[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    original.extend_from_slice(&data);

    let path = work_file("big.wav", &original);
    assert_eq!(sniff(&path).unwrap(), Container::Wav);
    assert!(matches!(write_file(&path, &sample_tags(), &[], &opts()), Err(Error::Unsupported(_))));
    assert_eq!(fs::read(&path).unwrap(), original);
    cleanup(&path);
}

#[test]
fn wav_with_a_truncated_chunk_is_malformed() {
    let mut original =
        riff_file(b"RIFF", b"WAVE", false, &[wav_fmt(), chunk(b"data", &wav_samples(), false)]);
    original.truncate(original.len() - 10);
    let path = work_file("cut.wav", &original);
    assert!(matches!(write_file(&path, &sample_tags(), &[], &opts()), Err(Error::Malformed(_))));
    assert_eq!(fs::read(&path).unwrap(), original);
    cleanup(&path);
}

#[test]
fn wav_ffmpeg_fixtures_round_trip() {
    for name in ["riffaiffdsf-ffmpeg.wav", "riffaiffdsf-lavf-info.wav"] {
        let original = fs::read(fixture(name)).unwrap();
        let path = work_file(name, &original);
        let before_audio = audio_bytes(&path, "wav");
        let frames = decode_all(&path, "wav");

        write_file(&path, &sample_tags(), &[cover(b"cover")], &opts()).unwrap();

        let (old_chunks, _) = chunks_of(&original, false);
        let (new_chunks, junk) = chunks_of(&fs::read(&path).unwrap(), false);
        assert!(junk.is_empty());
        for id in [b"fmt ", b"data"] {
            assert_eq!(find(&old_chunks, id), find(&new_chunks, id));
        }
        assert_eq!(find(&new_chunks, b"id3 ").len(), 1);

        let back = read_back(&path, "wav");
        assert_eq!(back.std, sample_std());
        assert_eq!(back.visuals.len(), 1);
        assert_eq!(audio_bytes(&path, "wav"), before_audio);
        assert_eq!(decode_all(&path, "wav"), frames);

        cleanup(&path);
    }
}

#[test]
fn wav_honours_the_id3v2_version() {
    let path = work_file("v3.wav", &rich_wav(b"id3 "));
    let v3 = WriteOptions { id3v2_version: Id3v2Version::V3, padding: 0, ..Default::default() };
    write_file(&path, &sample_tags(), &[], &v3).unwrap();
    let (chunks, _) = chunks_of(&fs::read(&path).unwrap(), false);
    assert_eq!(&find(&chunks, b"id3 ")[0].payload[..4], b"ID3\x03");
    cleanup(&path);

    // The default policy keeps v2.4 for a v2.4 tag.
    let path = work_file("v4.wav", &rich_wav(b"id3 "));
    write_file(&path, &sample_tags(), &[], &WriteOptions::default()).unwrap();
    let (chunks, _) = chunks_of(&fs::read(&path).unwrap(), false);
    assert_eq!(&find(&chunks, b"id3 ")[0].payload[..4], b"ID3\x04");
    cleanup(&path);
}

// ---------------------------------------------------------------------------------------------
// AIFF.
// ---------------------------------------------------------------------------------------------

#[test]
fn aiff_rewrites_tags_and_preserves_everything_else() {
    for id3_id in [b"ID3 ", b"id3 "] {
        let original = rich_aiff(id3_id);
        let path = work_file("rich.aiff", &original);

        let before_audio = audio_bytes(&path, "aiff");
        let frames = decode_all(&path, "aiff");

        let visuals = [cover(b"\x89PNG-aiff")];
        write_file(&path, &sample_tags(), &visuals, &opts()).unwrap();

        let out = fs::read(&path).unwrap();
        let (old_chunks, _) = chunks_of(&original, true);
        let (new_chunks, junk) = chunks_of(&out, true);

        assert!(junk.is_empty());
        assert_eq!(&out[0..4], b"FORM");
        assert_eq!(u32::from_be_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert_eq!(&out[8..12], b"AIFF");

        let is_tag = |c: &RawChunk| {
            c.id == *b"ID3 "
                || c.id == *b"id3 "
                || [*b"NAME", *b"AUTH", *b"(c) ", *b"ANNO"].contains(&c.id)
        };
        let kept = |chunks: &[RawChunk]| -> Vec<RawChunk> {
            chunks.iter().filter(|c| !is_tag(c)).cloned().collect()
        };
        assert_eq!(kept(&old_chunks), kept(&new_chunks));

        // The ID3 chunk replaced the old one in place, and kept its unmanaged frames.
        let id3: Vec<_> =
            new_chunks.iter().filter(|c| c.id == *b"ID3 " || c.id == *b"id3 ").collect();
        assert_eq!(id3.len(), 1);
        assert_eq!(id3[0].id, *b"ID3 ");
        assert!(contains(&id3[0].payload, b"PRIV"));
        assert!(contains(&id3[0].payload, b"POPM"));
        assert!(!contains(&id3[0].payload, b"Old Title"));
        assert_eq!(new_chunks.last().unwrap().id, *b"ID3 ");

        // The text chunks were rewritten.
        assert_eq!(find(&new_chunks, b"NAME")[0].payload, b"New Title");
        assert_eq!(find(&new_chunks, b"AUTH")[0].payload, b"New Artist");
        assert_eq!(find(&new_chunks, b"ANNO")[0].payload, b"New comment");
        assert_eq!(find(&new_chunks, b"NAME").len(), 1);

        let back = read_back(&path, "aiff");
        let mut expected = sample_std();
        expected.extend(
            back.std
                .iter()
                .filter(|t| t.starts_with("Rating") || t.starts_with("PlayCounter"))
                .cloned(),
        );
        assert_eq!(back.std, expected);
        assert_eq!(
            back.visuals,
            vec![(Some(StandardVisualKey::FrontCover), visuals[0].data.to_vec())]
        );

        assert_eq!(audio_bytes(&path, "aiff"), before_audio);
        assert_eq!(decode_all(&path, "aiff"), frames);

        cleanup(&path);
    }
}

#[test]
fn aiff_without_tags_gets_chunks_at_the_end_and_empty_tags_remove_them() {
    let original = riff_file(b"FORM", b"AIFC", true, &[aiff_comm_aifc(), aiff_ssnd()]);
    let path = work_file("bare.aifc", &original);

    write_file(&path, &sample_tags(), &[cover(b"cover")], &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let (chunks, junk) = chunks_of(&out, true);
    assert!(junk.is_empty());
    let ids: Vec<_> = chunks.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![*b"COMM", *b"SSND", *b"NAME", *b"AUTH", *b"ANNO", *b"ID3 "]);
    assert_eq!(u32::from_be_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
    assert_eq!(&out[8..12], b"AIFC");

    let back = read_back(&path, "aifc");
    assert_eq!(back.std, sample_std());
    assert_eq!(back.visuals, vec![(Some(StandardVisualKey::FrontCover), b"cover".to_vec())]);

    // Removing every tag restores the original file.
    write_file(&path, &[], &[], &opts()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);

    cleanup(&path);
}

/// COMM chunk of an AIFC file (uncompressed `NONE`).
fn aiff_comm_aifc() -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&1i16.to_be_bytes());
    c.extend_from_slice(&200u32.to_be_bytes());
    c.extend_from_slice(&16i16.to_be_bytes());
    c.extend_from_slice(&[0x40, 0x0b, 0xfa, 0x00, 0, 0, 0, 0, 0, 0]);
    c.extend_from_slice(b"NONE");
    c.extend_from_slice(b"\x0enot compressed\0");
    chunk(b"COMM", &c, true)
}

#[test]
fn aiff_ffmpeg_fixture_round_trips() {
    let original = fs::read(fixture("riffaiffdsf-ffmpeg.aiff")).unwrap();
    let path = work_file("ffmpeg.aiff", &original);
    let before_audio = audio_bytes(&path, "aiff");
    let frames = decode_all(&path, "aiff");

    write_file(&path, &sample_tags(), &[cover(b"cover")], &opts()).unwrap();

    let (old_chunks, _) = chunks_of(&original, true);
    let (new_chunks, junk) = chunks_of(&fs::read(&path).unwrap(), true);
    assert!(junk.is_empty());
    for id in [b"COMM", b"SSND"] {
        assert_eq!(find(&old_chunks, id), find(&new_chunks, id));
    }

    let back = read_back(&path, "aiff");
    assert_eq!(back.std, sample_std());
    assert_eq!(back.visuals.len(), 1);
    assert_eq!(audio_bytes(&path, "aiff"), before_audio);
    assert_eq!(decode_all(&path, "aiff"), frames);

    cleanup(&path);
}

#[test]
fn aiff_that_is_not_aiff_is_rejected() {
    let path = work_file("bad.aiff", b"FORM\0\0\0\x04AIFX");
    assert!(write_file(&path, &sample_tags(), &[], &opts()).is_err());
    cleanup(&path);
}

// ---------------------------------------------------------------------------------------------
// DSF.
// ---------------------------------------------------------------------------------------------

#[test]
fn dsf_adds_a_tag_and_removes_it_again() {
    let original = dsf_file(&dsf_data(), None);
    let path = work_file("plain.dsf", &original);
    assert_eq!(sniff(&path).unwrap(), Container::Dsf);
    let before_audio = audio_bytes(&path, "dsf");
    assert!(!before_audio.is_empty());

    let visuals = [cover(b"\x89PNG-dsf")];
    write_file(&path, &sample_tags(), &visuals, &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let data_end = 80 + 12 + dsf_data().len();
    assert_eq!(&out[..12], &original[..12]);
    assert_eq!(&out[28..data_end], &original[28..data_end]);
    assert_eq!(le_u64(&out[12..20]), out.len() as u64);
    assert_eq!(le_u64(&out[20..28]), data_end as u64);
    assert_eq!(&out[data_end..data_end + 3], b"ID3");

    let back = read_back(&path, "dsf");
    assert_eq!(back.std, sample_std());
    assert_eq!(back.visuals, vec![(Some(StandardVisualKey::FrontCover), visuals[0].data.to_vec())]);
    assert_eq!(audio_bytes(&path, "dsf"), before_audio);

    // Removing every tag restores the original file, with no metadata pointer.
    write_file(&path, &[], &[], &opts()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);

    cleanup(&path);
}

#[test]
fn dsf_replaces_a_tag_and_keeps_its_unmanaged_frames() {
    let original = dsf_file(&dsf_data(), Some(&old_id3_with_priv_popm()));
    let path = work_file("tagged.dsf", &original);
    let before_audio = audio_bytes(&path, "dsf");

    write_file(&path, &sample_tags(), &[], &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let data_end = 80 + 12 + dsf_data().len();
    assert_eq!(&out[28..data_end], &original[28..data_end]);
    assert_eq!(le_u64(&out[12..20]), out.len() as u64);
    assert_eq!(le_u64(&out[20..28]), data_end as u64);

    let tag = &out[data_end..];
    assert!(contains(tag, b"PRIV"));
    assert!(contains(tag, b"com.example.owner"));
    assert!(contains(tag, b"POPM"));
    assert!(contains(tag, b"New Title"));
    assert!(!contains(tag, b"Old Title"));

    let back = read_back(&path, "dsf");
    let mut expected = sample_std();
    expected.extend(
        back.std
            .iter()
            .filter(|t| t.starts_with("Rating") || t.starts_with("PlayCounter"))
            .cloned(),
    );
    assert_eq!(back.std, expected);
    assert_eq!(audio_bytes(&path, "dsf"), before_audio);

    // A tag with nothing worth keeping is removed with the pointer.
    let original = dsf_file(&dsf_data(), Some(&id3_tag(&[(b"TIT2", text_frame("Old"))])));
    let path2 = work_file("remove.dsf", &original);
    write_file(&path2, &[], &[], &opts()).unwrap();
    let out = fs::read(&path2).unwrap();
    assert_eq!(out.len(), data_end);
    assert_eq!(le_u64(&out[12..20]), data_end as u64);
    assert_eq!(le_u64(&out[20..28]), 0);
    assert_eq!(&out[28..], &original[28..data_end]);
    assert!(read_back(&path2, "dsf").std.is_empty());

    cleanup(&path);
    cleanup(&path2);
}

#[test]
fn dsf_with_an_odd_data_size_keeps_the_tag_pointer_exact() {
    let mut data = dsf_data();
    data.push(0xaa);
    let original = dsf_file(&data, Some(&id3_tag(&[(b"TIT2", text_frame("Old"))])));
    let path = work_file("odd.dsf", &original);

    write_file(&path, &sample_tags(), &[], &opts()).unwrap();

    let out = fs::read(&path).unwrap();
    let data_end = 80 + 12 + data.len();
    assert_eq!(le_u64(&out[20..28]), data_end as u64);
    assert_eq!(le_u64(&out[12..20]), out.len() as u64);
    assert_eq!(&out[28..data_end], &original[28..data_end]);
    assert_eq!(&out[data_end..data_end + 3], b"ID3");
    assert_eq!(read_back(&path, "dsf").std, sample_std());

    cleanup(&path);
}

#[test]
fn dsf_with_a_bad_header_is_malformed() {
    let mut original = dsf_file(&dsf_data(), None);
    original.truncate(100);
    let path = work_file("cut.dsf", &original);
    assert!(matches!(write_file(&path, &sample_tags(), &[], &opts()), Err(Error::Malformed(_))));
    assert_eq!(fs::read(&path).unwrap(), original);
    cleanup(&path);
}
