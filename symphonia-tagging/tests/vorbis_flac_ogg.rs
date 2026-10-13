// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip tests of the FLAC and Ogg (Vorbis, Opus, FLAC-in-Ogg, Speex) tag writers: what is
//! written is read back through Symphonia's default probe, the audio is unchanged, and the
//! structure of the file (metadata block chain, page checksums and sequence numbers) is valid.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawTag, StandardTag, StandardVisualKey, Tag, Visual};
use symphonia_format_ogg::writer::page_crc;
use symphonia_tagging::{WriteOptions, write_file};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// --- Helpers ---------------------------------------------------------------------------------

/// Copies a fixture into a fresh directory and returns its path.
fn fixture(test: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("symphonia-tagging-{}-{test}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(name);
    fs::copy(Path::new(FIXTURES).join(name), &dst).unwrap();
    dst
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

fn std_tag(key: &str, value: &str, std: StandardTag) -> Tag {
    Tag::new_std(RawTag::new(key, value), std)
}

/// Tags with multi-valued artists, MusicBrainz IDs, numbers and totals, unicode and a free-form
/// field.
fn rich_tags() -> Vec<Tag> {
    use StandardTag::*;

    let title = "Zażółć gęślą jaźń ♪ — 日本語";

    vec![
        std_tag("TITLE", title, TrackTitle(s(title))),
        std_tag("ARTIST", "Alice", Artist(s("Alice"))),
        std_tag("ARTIST", "Bob & Çelik", Artist(s("Bob & Çelik"))),
        std_tag("ALBUMARTIST", "Various Artists", AlbumArtist(s("Various Artists"))),
        std_tag("ALBUM", "An Album", Album(s("An Album"))),
        std_tag("TRACKNUMBER", "3/12", TrackNumber(3)),
        std_tag("TRACKNUMBER", "3/12", TrackTotal(12)),
        std_tag("DISCNUMBER", "1/2", DiscNumber(1)),
        std_tag("DISCNUMBER", "1/2", DiscTotal(2)),
        std_tag("DATE", "2024-05-06", RecordingDate(s("2024-05-06"))),
        std_tag("GENRE", "Rock", Genre(s("Rock"))),
        std_tag("GENRE", "Electronic", Genre(s("Electronic"))),
        std_tag("COMPOSER", "Carol", Composer(s("Carol"))),
        std_tag("COMMENT", "multi\nline comment", Comment(s("multi\nline comment"))),
        std_tag("BPM", "120", Bpm(120)),
        std_tag("COMPILATION", "1", CompilationFlag(true)),
        std_tag("RELEASECOUNTRY", "IT", ReleaseCountry(s("IT"))),
        std_tag("REPLAYGAIN_TRACK_GAIN", "-6.50 dB", ReplayGainTrackGain(s("-6.50 dB"))),
        std_tag(
            "MUSICBRAINZ_ALBUMID",
            "11111111-1111-4111-8111-111111111111",
            MusicBrainzAlbumId(s("11111111-1111-4111-8111-111111111111")),
        ),
        std_tag(
            "MUSICBRAINZ_ARTISTID",
            "22222222-2222-4222-8222-222222222222",
            MusicBrainzArtistId(s("22222222-2222-4222-8222-222222222222")),
        ),
        std_tag(
            "MUSICBRAINZ_ARTISTID",
            "33333333-3333-4333-8333-333333333333",
            MusicBrainzArtistId(s("33333333-3333-4333-8333-333333333333")),
        ),
        std_tag(
            "MUSICBRAINZ_TRACKID",
            "44444444-4444-4444-8444-444444444444",
            MusicBrainzTrackId(s("44444444-4444-4444-8444-444444444444")),
        ),
        std_tag(
            "MUSICBRAINZ_RELEASETRACKID",
            "55555555-5555-4555-8555-555555555555",
            MusicBrainzReleaseTrackId(s("55555555-5555-4555-8555-555555555555")),
        ),
        std_tag(
            "MUSICBRAINZ_RELEASEGROUPID",
            "66666666-6666-4666-8666-666666666666",
            MusicBrainzReleaseGroupId(s("66666666-6666-4666-8666-666666666666")),
        ),
        Tag::new_from_parts("TXXX:My Custom Field", "free ♫ form", None),
    ]
}

/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// A front cover, and a large (spanning many Ogg pages) back cover with a description.
fn rich_visuals() -> Vec<Visual> {
    let front = Visual {
        media_type: None,
        dimensions: None,
        color_mode: None,
        usage: Some(StandardVisualKey::FrontCover),
        tags: vec![],
        data: PNG.into(),
    };

    let mut seed = 0x1234_5678u32;
    let mut data = vec![0xff, 0xd8, 0xff, 0xe0];
    data.extend((0..100_000).map(|_| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    }));

    let back = Visual {
        media_type: Some("image/jpeg".into()),
        dimensions: None,
        color_mode: None,
        usage: Some(StandardVisualKey::BackCover),
        tags: vec![std_tag("DESCRIPTION", "The back ♪", StandardTag::Description(s("The back ♪")))],
        data: data.into(),
    };

    vec![front, back]
}

fn probe(path: &Path) -> Box<dyn FormatReader> {
    let file = fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap_or_else(|e| panic!("{} does not probe: {e}", path.display()))
}

/// A tag, as a comparable string.
fn tag_key(tag: &Tag) -> String {
    match &tag.std {
        Some(std) => format!("std:{std:?}"),
        None => format!("raw:{}={}", tag.raw.key.to_uppercase(), tag.raw.value),
    }
}

/// A visual, as comparable parts.
fn visual_key(visual: &Visual) -> (String, String, Vec<String>, Vec<u8>) {
    let mut tags: Vec<String> = visual.tags.iter().map(tag_key).collect();
    tags.sort();

    (
        format!("{:?}", visual.usage),
        visual.media_type.clone().unwrap_or_default(),
        tags,
        visual.data.to_vec(),
    )
}

#[derive(Debug, PartialEq)]
struct Meta {
    tags: Vec<String>,
    visuals: Vec<(String, String, Vec<String>, Vec<u8>)>,
}

/// Reads all tags and visuals (sorted) through the default probe.
fn read_meta(path: &Path) -> Meta {
    let mut format = probe(path);
    let mut metadata = format.metadata();

    let mut tags = Vec::new();
    let mut visuals = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            tags.extend(revision.media.tags.iter().map(tag_key));
            visuals.extend(revision.media.visuals.iter().map(visual_key));
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    tags.sort();
    visuals.sort();

    Meta { tags, visuals }
}

fn expected_meta(tags: &[Tag], visuals: &[Visual]) -> Meta {
    let mut tags: Vec<String> = tags.iter().map(tag_key).collect();
    tags.sort();

    // The expected raw key of a free-form tag is its upper-cased key without a `TXXX:` prefix.
    for tag in &mut tags {
        if let Some(rest) = tag.strip_prefix("raw:TXXX:") {
            *tag = format!("raw:{rest}");
        }
    }

    let mut visuals: Vec<_> = visuals
        .iter()
        .map(|v| {
            let mut key = visual_key(v);
            // The media type is detected when it is not given.
            if key.1.is_empty() {
                key.1 = "image/png".into();
            }
            key
        })
        .collect();
    visuals.sort();

    Meta { tags, visuals }
}

/// All packets of all tracks, as comparable parts.
fn read_packets(path: &Path) -> Vec<(u32, String, Vec<u8>)> {
    let mut format = probe(path);
    let mut packets = Vec::new();

    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                packets.push((packet.track_id, format!("{:?}", packet.pts), packet.data.to_vec()))
            }
            Ok(None) | Err(SymphoniaError::ResetRequired) => break,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }

    packets
}

/// Decodes the default audio track, returning the number of decoded packets.
fn decode_all(path: &Path) -> usize {
    let mut format = probe(path);

    let track = format.default_track(TrackType::Audio).expect("an audio track");
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            track.codec_params.as_ref().unwrap().audio().unwrap(),
            &AudioDecoderOptions::default(),
        )
        .expect("a decoder");

    let mut decoded = 0;

    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id == track_id {
            decoder.decode(&packet).unwrap_or_else(|e| panic!("decode failed: {e}"));
            decoded += 1;
        }
    }

    decoded
}

// FLAC structure.

struct FlacBlock {
    kind: u8,
    is_last: bool,
    body: Vec<u8>,
}

/// Parses a FLAC file (after any ID3v2 tag): the metadata blocks and the audio.
fn parse_flac(bytes: &[u8]) -> (Vec<FlacBlock>, Vec<u8>, bool) {
    let mut pos = 0;
    let mut had_id3 = false;

    if bytes.starts_with(b"ID3") {
        let size = bytes[6..10].iter().fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f));
        pos = 10 + size;
        had_id3 = true;
    }

    assert_eq!(&bytes[pos..pos + 4], b"fLaC");
    pos += 4;

    let mut blocks = Vec::new();

    loop {
        let head = bytes[pos];
        let len = usize::from(bytes[pos + 1]) << 16
            | usize::from(bytes[pos + 2]) << 8
            | usize::from(bytes[pos + 3]);

        blocks.push(FlacBlock {
            kind: head & 0x7f,
            is_last: head & 0x80 != 0,
            body: bytes[pos + 4..pos + 4 + len].to_vec(),
        });

        pos += 4 + len;

        if head & 0x80 != 0 {
            break;
        }
    }

    (blocks, bytes[pos..].to_vec(), had_id3)
}

fn check_flac_chain(blocks: &[FlacBlock]) {
    assert_eq!(blocks[0].kind, 0, "STREAMINFO is first");
    for (i, block) in blocks.iter().enumerate() {
        assert_eq!(block.is_last, i + 1 == blocks.len(), "only the final block is flagged last");
    }
}

fn flac_vendor(blocks: &[FlacBlock]) -> String {
    let body = &blocks.iter().find(|b| b.kind == 4).expect("a comment block").body;
    let len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    String::from_utf8(body[4..4 + len].to_vec()).unwrap()
}

// Ogg structure.

#[derive(Clone)]
struct OggPage {
    raw: Vec<u8>,
    flags: u8,
    granule: i64,
    serial: u32,
    sequence: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

/// Parses all pages of an Ogg file, verifying their checksums.
fn parse_ogg(bytes: &[u8]) -> Vec<OggPage> {
    let mut pages = Vec::new();
    let mut pos = 0;

    while pos < bytes.len() {
        assert_eq!(&bytes[pos..pos + 4], b"OggS", "page capture pattern at {pos}");

        let nseg = usize::from(bytes[pos + 26]);
        let lacing = bytes[pos + 27..pos + 27 + nseg].to_vec();
        let body_len: usize = lacing.iter().map(|&l| usize::from(l)).sum();
        let end = pos + 27 + nseg + body_len;
        let raw = bytes[pos..end].to_vec();

        let mut zeroed = raw.clone();
        zeroed[22..26].copy_from_slice(&[0; 4]);
        assert_eq!(
            page_crc(&zeroed),
            u32::from_le_bytes(raw[22..26].try_into().unwrap()),
            "page checksum at {pos}"
        );

        pages.push(OggPage {
            flags: raw[5],
            granule: i64::from_le_bytes(raw[6..14].try_into().unwrap()),
            serial: u32::from_le_bytes(raw[14..18].try_into().unwrap()),
            sequence: u32::from_le_bytes(raw[18..22].try_into().unwrap()),
            body: bytes[pos + 27 + nseg..end].to_vec(),
            lacing,
            raw,
        });

        pos = end;
    }

    pages
}

/// The pages of one logical stream.
fn stream_pages(pages: &[OggPage], serial: u32) -> Vec<OggPage> {
    pages.iter().filter(|p| p.serial == serial).cloned().collect()
}

/// Checks that the pages of every logical stream are numbered consecutively from 0, and that
/// the first page is a beginning of stream and the last one an end of stream (if the original
/// is).
fn check_ogg_sequence(pages: &[OggPage]) {
    let mut serials: Vec<u32> = pages.iter().map(|p| p.serial).collect();
    serials.dedup();

    for serial in serials {
        for (i, page) in stream_pages(pages, serial).iter().enumerate() {
            assert_eq!(page.sequence as usize, i, "sequence of stream {serial:#x}");
        }
    }
}

/// The pages of a stream that follow its headers: the first page that has a granule position
/// onward, as (flags without the beginning of stream, granule, lacing, body).
fn data_pages(pages: &[OggPage]) -> Vec<(u8, i64, Vec<u8>, Vec<u8>)> {
    pages
        .iter()
        .skip_while(|p| p.granule == 0 || p.granule == -1)
        .map(|p| (p.flags & !0x02, p.granule, p.lacing.clone(), p.body.clone()))
        .collect()
}

/// Reassembles the packets of a stream.
fn ogg_packets(pages: &[OggPage]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut partial = Vec::new();

    for page in pages {
        let mut offset = 0;
        for &seg in &page.lacing {
            partial.extend_from_slice(&page.body[offset..offset + usize::from(seg)]);
            offset += usize::from(seg);
            if seg < 255 {
                packets.push(std::mem::take(&mut partial));
            }
        }
    }

    packets
}

/// Checks the page structure of a written Ogg file against the original: valid checksums,
/// consecutive sequence numbers, headers end with a page boundary and the audio pages of the
/// first stream are unchanged.
fn check_ogg_against(original: &[u8], written: &[u8]) {
    let before = parse_ogg(original);
    let after = parse_ogg(written);

    check_ogg_sequence(&after);

    let serial = before[0].serial;

    assert_eq!(
        data_pages(&stream_pages(&before, serial)),
        data_pages(&stream_pages(&after, serial)),
        "audio pages"
    );

    // The last page of the stream keeps its end of stream flag.
    assert_eq!(
        stream_pages(&before, serial).last().unwrap().flags & 0x04,
        stream_pages(&after, serial).last().unwrap().flags & 0x04
    );
}

fn tag_and_check_ogg(test: &str, name: &str) -> PathBuf {
    let path = fixture(test, name);
    let original = fs::read(&path).unwrap();
    let original_packets = read_packets(&path);

    let tags = rich_tags();
    let visuals = rich_visuals();

    write_file(&path, &tags, &visuals, &WriteOptions::default()).unwrap();

    assert_eq!(read_meta(&path), expected_meta(&tags, &visuals), "{name}: tags read back");

    let written = fs::read(&path).unwrap();
    check_ogg_against(&original, &written);

    assert_eq!(read_packets(&path), original_packets, "{name}: audio packets");

    path
}

// --- FLAC ------------------------------------------------------------------------------------

#[test]
fn flac_round_trip_preserves_unmanaged_blocks_and_drops_id3v2() {
    let path = fixture("flac_rich", "vorbis-rich.flac");
    let original = fs::read(&path).unwrap();
    let (old_blocks, old_audio, had_id3) = parse_flac(&original);
    assert!(had_id3);

    let original_packets = read_packets(&path);

    let tags = rich_tags();
    let visuals = rich_visuals();

    write_file(&path, &tags, &visuals, &WriteOptions::default()).unwrap();

    assert_eq!(read_meta(&path), expected_meta(&tags, &visuals));

    let written = fs::read(&path).unwrap();
    assert!(written.starts_with(b"fLaC"), "the leading ID3v2 tag is dropped");

    let (blocks, audio, had_id3) = parse_flac(&written);
    assert!(!had_id3);
    check_flac_chain(&blocks);

    // The audio frames are identical.
    assert_eq!(audio, old_audio);
    assert_eq!(read_packets(&path), original_packets);
    assert!(decode_all(&path) > 0);

    // SEEKTABLE, APPLICATION and CUESHEET are preserved byte for byte, in order.
    let unmanaged = |blocks: &[FlacBlock]| -> Vec<(u8, Vec<u8>)> {
        blocks
            .iter()
            .filter(|b| matches!(b.kind, 0 | 2 | 3 | 5))
            .map(|b| (b.kind, b.body.clone()))
            .collect()
    };
    assert_eq!(unmanaged(&blocks), unmanaged(&old_blocks));
    assert!(blocks.iter().any(|b| b.kind == 2 && b.body.starts_with(b"aulo")));

    // The vendor string is kept.
    assert_eq!(flac_vendor(&blocks), flac_vendor(&old_blocks));

    // Two pictures, and padding as requested (the ID3v2 tag was dropped, so the audio moved).
    assert_eq!(blocks.iter().filter(|b| b.kind == 6).count(), 2);
    assert_eq!(blocks.last().unwrap().kind, 1);
    assert_eq!(blocks.last().unwrap().body.len(), WriteOptions::default().padding);
}

#[test]
fn flac_rewrite_reuses_space_so_the_audio_offset_stays() {
    let path = fixture("flac_offset", "vorbis-plain.flac");
    let original = fs::read(&path).unwrap();
    let (_, audio, _) = parse_flac(&original);

    let tags = rich_tags();
    write_file(&path, &tags, &[], &WriteOptions::default()).unwrap();
    let first = fs::read(&path).unwrap();

    // The original has 8 KiB of padding, which is enough for the new tags.
    assert_eq!(first.len(), original.len());

    // Writing the same tags again is a no-op.
    write_file(&path, &tags, &[], &WriteOptions::default()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), first);

    // Fewer tags: the freed space becomes padding, the audio does not move.
    write_file(&path, &tags[..2], &[], &WriteOptions::default()).unwrap();
    let second = fs::read(&path).unwrap();
    assert_eq!(second.len(), original.len());

    let (blocks, new_audio, _) = parse_flac(&second);
    check_flac_chain(&blocks);
    assert_eq!(new_audio, audio);

    // More than what fits: the padding option is used.
    let mut big = rich_visuals();
    big.truncate(2);
    write_file(&path, &tags, &big, &WriteOptions { padding: 777, ..Default::default() }).unwrap();

    let (blocks, new_audio, _) = parse_flac(&fs::read(&path).unwrap());
    check_flac_chain(&blocks);
    assert_eq!(new_audio, audio);
    assert_eq!(blocks.last().unwrap().kind, 1);
    assert_eq!(blocks.last().unwrap().body.len(), 777);
}

#[test]
fn flac_remove_all_tags() {
    let path = fixture("flac_remove", "vorbis-rich.flac");
    let original_packets = read_packets(&path);

    write_file(&path, &rich_tags(), &rich_visuals(), &WriteOptions::default()).unwrap();
    write_file(&path, &[], &[], &WriteOptions { padding: 0, ..Default::default() }).unwrap();

    let meta = read_meta(&path);
    assert!(meta.tags.is_empty(), "{:?}", meta.tags);
    assert!(meta.visuals.is_empty());

    let (blocks, _, _) = parse_flac(&fs::read(&path).unwrap());
    check_flac_chain(&blocks);
    assert!(blocks.iter().all(|b| b.kind != 6 && b.kind != 1));
    assert_eq!(flac_vendor(&blocks), "Lavf63.1.102");

    assert_eq!(read_packets(&path), original_packets);
    assert!(decode_all(&path) > 0);
}

#[test]
fn flac_without_a_comment_block_gets_a_default_vendor() {
    let path = fixture("flac_vendor", "vorbis-nocomment.flac");

    write_file(&path, &rich_tags(), &[], &WriteOptions::default()).unwrap();

    let (blocks, _, _) = parse_flac(&fs::read(&path).unwrap());
    assert!(flac_vendor(&blocks).starts_with("symphonia-tagging "));
    assert_eq!(read_meta(&path), expected_meta(&rich_tags(), &[]));
}

#[test]
fn flac_recording_id_is_written_as_the_track_id() {
    let path = fixture("flac_mbid", "vorbis-plain.flac");

    let id = "b9ad642e-b012-41c7-b72a-42cf4911f9ff";
    let tags = vec![std_tag("X", id, StandardTag::MusicBrainzRecordingId(s(id)))];

    write_file(&path, &tags, &[], &WriteOptions::default()).unwrap();

    let meta = read_meta(&path);
    assert_eq!(meta.tags, vec![format!("std:MusicBrainzTrackId({id:?})")]);
}

#[test]
fn flac_rejects_garbage() {
    let path = fixture("flac_garbage", "vorbis-plain.flac");
    let mut bytes = fs::read(&path).unwrap();
    bytes[4] = 0x7f;
    fs::write(&path, &bytes).unwrap();

    assert!(write_file(&path, &[], &[], &WriteOptions::default()).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes, "a failed write leaves the file alone");
}

// --- Ogg -------------------------------------------------------------------------------------

#[test]
fn ogg_vorbis_round_trip() {
    let path = tag_and_check_ogg("ogg_vorbis", "vorbis-plain.ogg");
    assert!(decode_all(&path) > 0);

    // The chapter comments of the original (which are not tags) are kept.
    let bytes = fs::read(&path).unwrap();
    let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(contains(b"CHAPTER001=00:00:00.000"));
    assert!(contains(b"CHAPTER001NAME=Intro"));
    assert!(contains(b"Lavf"), "the vendor string is kept");

    // The old tags are gone.
    assert!(!contains(b"Old title"));
}

#[test]
fn ogg_opus_round_trip() {
    let path = tag_and_check_ogg("ogg_opus", "vorbis-plain.opus");
    assert!(decode_all(&path) > 0);

    // The comment header is on its own pages: the first page holds OpusHead only.
    let pages = parse_ogg(&fs::read(&path).unwrap());
    assert!(pages[0].body.starts_with(b"OpusHead"));
    assert!(pages[1].body.starts_with(b"OpusTags"));
    assert_eq!(pages[0].flags & 0x02, 0x02);
}

#[test]
fn ogg_opus_keeps_trailing_binary_of_the_comment_header() {
    let path = fixture("ogg_opus_trailing", "vorbis-padded.opus");

    write_file(&path, &rich_tags(), &[], &WriteOptions::default()).unwrap();

    let packets = ogg_packets(&parse_ogg(&fs::read(&path).unwrap()));
    assert!(packets[1].starts_with(b"OpusTags"));
    assert!(packets[1].ends_with(b"\x00\x01TRAILING-BINARY\xff"));

    assert_eq!(read_meta(&path), expected_meta(&rich_tags(), &[]));
}

#[test]
fn ogg_flac_round_trip() {
    let path = tag_and_check_ogg("ogg_flac", "vorbis-plain-flac.oga");
    assert!(decode_all(&path) > 0);

    // The identification packet counts the new metadata packets: comment and two pictures.
    let pages = parse_ogg(&fs::read(&path).unwrap());
    let packets = ogg_packets(&pages);
    assert!(packets[0].starts_with(b"\x7fFLAC"));
    let count = usize::from(u16::from_be_bytes([packets[0][7], packets[0][8]]));
    assert_eq!(count, 3);

    let blocks = &packets[1..1 + count];
    assert_eq!(blocks[0][0] & 0x7f, 4);
    assert_eq!(blocks[1][0] & 0x7f, 6);
    assert_eq!(blocks[2][0], 0x80 | 6, "the final block is flagged last");
}

#[test]
fn ogg_vorbis_remove_all_tags() {
    for name in ["vorbis-plain.ogg", "vorbis-plain.opus", "vorbis-plain-flac.oga"] {
        let path = fixture("ogg_remove", name);
        let original = fs::read(&path).unwrap();
        let original_packets = read_packets(&path);

        write_file(&path, &rich_tags(), &rich_visuals(), &WriteOptions::default()).unwrap();
        write_file(&path, &[], &[], &WriteOptions::default()).unwrap();

        let meta = read_meta(&path);
        assert!(meta.tags.is_empty(), "{name}: {:?}", meta.tags);
        assert!(meta.visuals.is_empty(), "{name}");

        check_ogg_against(&original, &fs::read(&path).unwrap());
        assert_eq!(read_packets(&path), original_packets, "{name}");
        assert!(decode_all(&path) > 0, "{name}");
    }
}

#[test]
fn ogg_other_streams_are_copied_untouched() {
    let path = fixture("ogg_multiplexed", "vorbis-two-streams.ogg");
    let original = fs::read(&path).unwrap();
    let before = parse_ogg(&original);

    let serials: Vec<u32> = {
        let mut s: Vec<u32> = before.iter().map(|p| p.serial).collect();
        s.dedup();
        s.sort_unstable();
        s.dedup();
        s
    };
    assert_eq!(serials.len(), 2);

    write_file(&path, &rich_tags(), &rich_visuals(), &WriteOptions::default()).unwrap();

    let after = parse_ogg(&fs::read(&path).unwrap());
    check_ogg_sequence(&after);

    let first = before[0].serial;
    let other = *serials.iter().find(|&&s| s != first).unwrap();

    let raw = |pages: &[OggPage]| -> Vec<Vec<u8>> {
        stream_pages(pages, other).into_iter().map(|p| p.raw).collect()
    };
    assert_eq!(raw(&before), raw(&after));

    assert_eq!(data_pages(&stream_pages(&before, first)), data_pages(&stream_pages(&after, first)));

    assert_eq!(read_meta(&path).tags.iter().filter(|t| t.starts_with("std:Artist(")).count(), 2);
}

#[test]
fn ogg_chained_file_rewrites_the_first_link_only() {
    let path = fixture("ogg_chained", "vorbis-plain.ogg");
    let second = fs::read(Path::new(FIXTURES).join("vorbis-plain.opus")).unwrap();

    let mut chained = fs::read(&path).unwrap();
    let first_len = chained.len();
    chained.extend_from_slice(&second);
    fs::write(&path, &chained).unwrap();

    write_file(&path, &rich_tags(), &[], &WriteOptions::default()).unwrap();

    let written = fs::read(&path).unwrap();
    assert!(written.ends_with(&second), "the second link is untouched");

    check_ogg_against(&chained[..first_len], &written[..written.len() - second.len()]);
}

#[test]
fn ogg_speex_comment_is_rewritten() {
    let path = fixture("ogg_speex", "vorbis-plain.spx");
    let original = fs::read(&path).unwrap();

    write_file(&path, &rich_tags(), &[], &WriteOptions::default()).unwrap();

    let written = fs::read(&path).unwrap();
    check_ogg_against(&original, &written);

    let packets = ogg_packets(&parse_ogg(&written));
    assert!(packets[0].starts_with(b"Speex   "));

    let comment = &packets[1];
    let contains = |needle: &[u8]| comment.windows(needle.len()).any(|w| w == needle);
    assert!(contains(b"ARTIST=Alice"));
    assert!(contains(b"TRACKNUMBER=3"));
    assert!(!contains(b"Old title"));
    assert!(contains(b"Lavf"), "the vendor string is kept");
}

#[test]
fn ogg_rejects_what_it_cannot_tag() {
    let path = fixture("ogg_bad", "vorbis-plain.ogg");
    let mut bytes = fs::read(&path).unwrap();
    // Corrupt the identification packet signature.
    bytes[28] = b'X';
    fs::write(&path, &bytes).unwrap();

    assert!(write_file(&path, &[], &[], &WriteOptions::default()).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
