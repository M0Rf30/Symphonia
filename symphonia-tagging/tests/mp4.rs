// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round trips through the MP4 writer: every file written is read back through the default
//! probe, and must give the same tags and pictures, the same packets (so that the chunk offsets
//! that were moved are right) and the same decoded audio.
//!
//! The `mp4-*` fixtures are one to two seconds of a mono sine, made with
//!
//! ```text
//! ffmpeg -f lavfi -i sine=frequency=440:duration=2:sample_rate=22050 -ac 1 -map_metadata -1 \
//!     -fflags +bitexact -flags:a +bitexact -c:a aac -b:a 24k <flags> <file>
//! ```
//!
//! with `-movflags +faststart` (`mp4-aac-faststart`), nothing (`mp4-aac-moov-end`),
//! `-movflags frag_keyframe+empty_moov+default_base_moof` (`mp4-aac-fragmented`), `-map 0:a
//! -map 0:a -b:a 16k -movflags +faststart` (`mp4-aac-two-tracks`, 45 interleaved chunks per
//! track) and `-c:a alac -movflags +faststart` with a duration of 1 second (`mp4-alac`). The
//! `mp4-pretagged*` files are the faststart and moov-at-end files tagged with mutagen: iTunes
//! atoms, free-form atoms (one with two values, one in another namespace), two covers and two
//! atoms (`xxxx`, `zzzz`) that are not tags and must survive a rewrite.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    ContentAdvisory, MetadataOptions, RawTag, RawValue, StandardTag, StandardVisualKey, Tag, Visual,
};
use symphonia_tagging::{Error, WriteOptions, write_file};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A copy of a fixture in a directory of its own, removed when dropped.
struct Work {
    dir: PathBuf,
    path: PathBuf,
}

impl Work {
    fn new(fixture_name: &str, test: &str) -> Work {
        let dir = std::env::temp_dir()
            .join(format!("symphonia-tagging-mp4-{}-{test}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(fixture_name);
        fs::copy(fixture(fixture_name), &path).unwrap();
        Work { dir, path }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(&self.path).unwrap()
    }

    fn set_bytes(&self, bytes: &[u8]) {
        fs::write(&self.path, bytes).unwrap();
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn s(v: &str) -> Arc<String> {
    Arc::new(v.to_string())
}

/// What a file reads as.
struct Probed {
    tags: Vec<Tag>,
    visuals: Vec<Visual>,
    /// Track, presentation time and payload of every packet.
    packets: Vec<(u32, String, Vec<u8>)>,
    /// A checksum of the decoded samples of each track.
    pcm: Vec<(u32, u64)>,
}

fn probe(path: &Path) -> Probed {
    let file = File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("m4a");

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .expect("the file should probe");

    let mut tags = Vec::new();
    let mut visuals = Vec::new();

    loop {
        let mut metadata = format.metadata();

        if let Some(revision) = metadata.current() {
            tags.extend(revision.media.tags.iter().cloned());
            visuals.extend(revision.media.visuals.iter().cloned());
        }

        if metadata.is_latest() {
            break;
        }

        metadata.pop();
    }

    let mut decoders: HashMap<u32, Box<dyn AudioDecoder>> = HashMap::new();

    for track in format.tracks() {
        if let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) {
            if let Ok(decoder) = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
            {
                decoders.insert(track.id, decoder);
            }
        }
    }

    assert!(!decoders.is_empty(), "the file should have a decodable audio track");

    let mut packets = Vec::new();
    let mut hashes: HashMap<u32, u64> = HashMap::new();

    while let Some(packet) = format.next_packet().expect("packets should read") {
        packets.push((packet.track_id, format!("{:?}", packet.pts), packet.data.to_vec()));

        if let Some(decoder) = decoders.get_mut(&packet.track_id) {
            let decoded = decoder.decode(&packet).expect("packets should decode");

            let mut samples: Vec<f32> = Vec::new();
            decoded.copy_to_vec_interleaved(&mut samples);

            let hash = hashes.entry(packet.track_id).or_insert(0xcbf2_9ce4_8422_2325);
            for sample in samples {
                for byte in sample.to_bits().to_le_bytes() {
                    *hash = (*hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
    }

    let mut pcm: Vec<(u32, u64)> = hashes.into_iter().collect();
    pcm.sort();

    Probed { tags, visuals, packets, pcm }
}

/// The standard tags of `tags`, as a sorted list. The recording id and the track id are one
/// field in MP4, which reads back as the track id.
fn std_tags(tags: &[Tag]) -> Vec<String> {
    let mut out: Vec<String> = tags
        .iter()
        .filter_map(|t| t.std.clone())
        .map(|std| match std {
            StandardTag::MusicBrainzRecordingId(v) => StandardTag::MusicBrainzTrackId(v),
            std => std,
        })
        .map(|std| format!("{std:?}"))
        .collect();
    out.sort();
    out
}

/// The tags that have no standard tag, as key/value pairs, with `key` applied to the keys.
fn raw_only(tags: &[Tag], key: impl Fn(&str) -> String) -> Vec<String> {
    let mut out: Vec<String> = tags
        .iter()
        .filter(|t| t.std.is_none())
        .map(|t| format!("{}={}", key(&t.raw.key), t.raw.value))
        .collect();
    out.sort();
    out
}

/// Everything about the tags of a file, as a sorted list.
fn everything(tags: &[Tag]) -> Vec<String> {
    let mut out: Vec<String> =
        tags.iter().map(|t| format!("{}={}|{:?}", t.raw.key, t.raw.value, t.std)).collect();
    out.sort();
    out
}

fn visual_data(visuals: &[Visual]) -> Vec<Vec<u8>> {
    visuals.iter().map(|v| v.data.to_vec()).collect()
}

fn jpeg() -> Vec<u8> {
    fs::read(fixture("mp4-cover.jpg")).unwrap()
}

fn png() -> Vec<u8> {
    fs::read(fixture("mp4-cover.png")).unwrap()
}

fn visual(data: Vec<u8>) -> Visual {
    Visual {
        media_type: None,
        dimensions: None,
        color_mode: None,
        usage: Some(StandardVisualKey::FrontCover),
        tags: Vec::new(),
        data: data.into_boxed_slice(),
    }
}

fn tag(key: &str, std: StandardTag) -> Tag {
    Tag::new_std(RawTag::new(key, key), std)
}

/// A tag set that reaches most of the mapping.
fn rich_tags() -> Vec<Tag> {
    use StandardTag as S;

    let mut tags = vec![
        tag("TITLE", S::TrackTitle(s("A Title"))),
        tag("ARTIST", S::Artist(s("First Artist"))),
        tag("ARTIST", S::Artist(s("Second Artist"))),
        tag("ALBUMARTIST", S::AlbumArtist(s("The Album Artist"))),
        tag("ALBUM", S::Album(s("An Album"))),
        tag("DATE", S::ReleaseDate(s("2023-07-08"))),
        tag("GENRE", S::Genre(s("Electronic"))),
        tag("TRACKNUMBER", S::TrackNumber(4)),
        tag("TRACKTOTAL", S::TrackTotal(10)),
        tag("DISCNUMBER", S::DiscNumber(2)),
        tag("DISCTOTAL", S::DiscTotal(3)),
        tag("COMPILATION", S::CompilationFlag(true)),
        tag("BPM", S::Bpm(174)),
        tag("COMMENT", S::Comment(s("Some comment"))),
        tag("LYRICS", S::Lyrics(s("one\ntwo"))),
        tag("COMPOSER", S::Composer(s("A Composer"))),
        tag("GROUPING", S::Grouping(s("A Group"))),
        tag("COPYRIGHT", S::Copyright(s("(c) 2023"))),
        tag("TITLESORT", S::SortTrackTitle(s("Title, A"))),
        tag("ARTISTSORT", S::SortArtist(s("Artist, First"))),
        tag("ALBUMSORT", S::SortAlbum(s("Album, An"))),
        tag(
            "MUSICBRAINZ_TRACKID",
            S::MusicBrainzRecordingId(s("10000000-0000-0000-0000-000000000001")),
        ),
        tag(
            "MUSICBRAINZ_ALBUMID",
            S::MusicBrainzAlbumId(s("20000000-0000-0000-0000-000000000002")),
        ),
        tag(
            "MUSICBRAINZ_ARTISTID",
            S::MusicBrainzArtistId(s("30000000-0000-0000-0000-000000000003")),
        ),
        tag(
            "MUSICBRAINZ_RELEASETRACKID",
            S::MusicBrainzReleaseTrackId(s("40000000-0000-0000-0000-000000000004")),
        ),
        tag(
            "MUSICBRAINZ_RELEASEGROUPID",
            S::MusicBrainzReleaseGroupId(s("50000000-0000-0000-0000-000000000005")),
        ),
        tag("ISRC", S::IdentIsrc(s("USRC17607839"))),
        tag("LABEL", S::Label(s("A Label"))),
        tag("BARCODE", S::IdentBarcode(s("0123456789012"))),
        tag("RATING", S::Rating(60 * 10_000)),
        tag("ADVISORY", S::ContentAdvisory(ContentAdvisory::Explicit)),
    ];

    tags.push(Tag::new(RawTag::new("MY CUSTOM FIELD", "custom value")));
    tags.push(Tag::new(RawTag::new("MY CUSTOM FIELD", "second value")));
    tags.push(Tag::new(RawTag::new("org.example:Namespaced", "ns value")));
    tags
}

// ---------------------------------------------------------------------------------------------
// Box helpers: just enough to look at the structure of a file.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct B {
    start: usize,
    end: usize,
    header: usize,
    kind: [u8; 4],
}

fn boxes(bytes: &[u8], start: usize, end: usize) -> Vec<B> {
    let mut out = Vec::new();
    let mut pos = start;

    while end - pos >= 8 {
        let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = bytes[pos + 4..pos + 8].try_into().unwrap();

        let (header, size) = match size32 {
            0 => (8, end - pos),
            1 => (16, u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize),
            n => (8, n),
        };

        out.push(B { start: pos, end: pos + size, header, kind });
        pos += size;
    }

    out
}

fn top(bytes: &[u8]) -> Vec<B> {
    boxes(bytes, 0, bytes.len())
}

fn child(bytes: &[u8], parent: B, kind: &[u8; 4]) -> Option<B> {
    // `meta` is a full box.
    let prefix = if &parent.kind == b"meta" { 4 } else { 0 };
    boxes(bytes, parent.start + parent.header + prefix, parent.end)
        .into_iter()
        .find(|b| &b.kind == kind)
}

fn find(bytes: &[u8], path: &[&[u8; 4]]) -> Option<B> {
    let mut current = top(bytes).into_iter().find(|b| &b.kind == path[0])?;
    for kind in &path[1..] {
        current = child(bytes, current, kind)?;
    }
    Some(current)
}

fn kinds(bytes: &[u8]) -> Vec<String> {
    top(bytes).iter().map(|b| String::from_utf8_lossy(&b.kind).into_owned()).collect()
}

/// The `moof` offsets in the `tfra` of the file's `mfra` (versions 0 and 1).
fn tfra_offsets(bytes: &[u8]) -> Vec<usize> {
    let tfra = find(bytes, &[b"mfra", b"tfra"]).expect("a tfra");
    let body = &bytes[tfra.start + tfra.header..tfra.end];

    // Version, flags, track id, the length sizes of the numbers, the entry count.
    let wide = body[0] == 1;
    let numbers = {
        let sizes = u32::from_be_bytes(body[8..12].try_into().unwrap());
        (((sizes >> 4) & 3) + ((sizes >> 2) & 3) + (sizes & 3) + 3) as usize
    };
    let count = u32::from_be_bytes(body[12..16].try_into().unwrap()) as usize;
    let field = if wide { 8 } else { 4 };

    (0..count)
        .map(|i| {
            let at = 16 + i * (2 * field + numbers) + field;
            if wide {
                u64::from_be_bytes(body[at..at + 8].try_into().unwrap()) as usize
            }
            else {
                u32::from_be_bytes(body[at..at + 4].try_into().unwrap()) as usize
            }
        })
        .collect()
}

fn slice(bytes: &[u8], b: B) -> &[u8] {
    &bytes[b.start..b.end]
}

/// The atoms of the `ilst`: kind and bytes.
fn ilst_atoms(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let ilst = find(bytes, &[b"moov", b"udta", b"meta", b"ilst"]).expect("an ilst");
    boxes(bytes, ilst.start + ilst.header, ilst.end)
        .into_iter()
        .map(|b| (b.kind, slice(bytes, b).to_vec()))
        .collect()
}

fn write(path: &Path, tags: &[Tag], visuals: &[Visual]) {
    write_file(path, tags, visuals, &WriteOptions::default()).expect("the write should succeed");
}

fn write_with(path: &Path, tags: &[Tag], visuals: &[Visual], padding: usize) {
    let opts = WriteOptions { padding, ..WriteOptions::default() };
    write_file(path, tags, visuals, &opts).expect("the write should succeed");
}

/// Checks the properties every rewrite must keep: the audio is the same (bytes, packets and
/// decoded samples), and the tags read back as the ones written.
fn assert_rewrite(
    before: &Probed,
    before_bytes: &[u8],
    after_path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
) {
    let after = probe(after_path);
    let after_bytes = fs::read(after_path).unwrap();

    assert_eq!(std_tags(&after.tags), std_tags(tags), "standard tags");
    // Free-form keys read back with their namespace; the default one is iTunes'.
    let namespaced = |key: &str| {
        if key.starts_with("org.example:") {
            key.to_string()
        }
        else {
            format!("com.apple.iTunes:{key}")
        }
    };
    assert_eq!(raw_only(&after.tags, str::to_string), raw_only(tags, namespaced), "free-form tags");
    assert_eq!(visual_data(&after.visuals), visual_data(visuals), "pictures");

    assert_eq!(after.packets.len(), before.packets.len());
    assert!(after.packets == before.packets, "the packets changed");
    assert_eq!(after.pcm, before.pcm, "the decoded audio changed");

    let before_mdat: Vec<&[u8]> = top(before_bytes)
        .into_iter()
        .filter(|b| &b.kind == b"mdat")
        .map(|b| slice(before_bytes, b))
        .collect();
    let after_mdat: Vec<&[u8]> = top(&after_bytes)
        .into_iter()
        .filter(|b| &b.kind == b"mdat")
        .map(|b| slice(&after_bytes, b))
        .collect();
    assert_eq!(before_mdat, after_mdat, "the media data changed");
}

// ---------------------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------------------

#[test]
fn faststart_file_grows_and_the_chunk_offsets_follow() {
    let work = Work::new("mp4-aac-faststart.m4a", "faststart");
    let before = probe(&work.path);
    let before_bytes = work.bytes();
    assert!(before.tags.is_empty());

    let visuals = [visual(jpeg()), visual(png())];
    write(&work.path, &rich_tags(), &visuals);

    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &visuals);

    let bytes = work.bytes();
    assert_eq!(kinds(&bytes), ["ftyp", "moov", "free", "mdat"]);

    // The moov grew, and the default padding was left behind it for the next edit.
    let moov = find(&bytes, &[b"moov"]).unwrap();
    let old_moov = find(&before_bytes, &[b"moov"]).unwrap();
    assert!(moov.end - moov.start > old_moov.end - old_moov.start + 500);
    let free = find(&bytes, &[b"free"]).unwrap();
    assert_eq!(free.end - free.start, 4096);

    // The mdat moved by exactly what the moov and the padding took.
    let old_mdat = find(&before_bytes, &[b"mdat"]).unwrap();
    let mdat = find(&bytes, &[b"mdat"]).unwrap();
    assert_eq!(
        mdat.start - old_mdat.start,
        (moov.end - moov.start + 4096) - (old_moov.end - old_moov.start + 8)
    );
}

#[test]
fn a_later_write_that_fits_the_padding_moves_nothing() {
    let work = Work::new("mp4-aac-faststart.m4a", "padding");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    let visuals = [visual(jpeg())];
    write(&work.path, &rich_tags(), &visuals);
    let grown = work.bytes();
    let mdat = find(&grown, &[b"mdat"]).unwrap();

    // Another edit, a little smaller.
    let mut tags = rich_tags();
    tags.truncate(20);
    write(&work.path, &tags, &visuals);

    let bytes = work.bytes();
    assert_eq!(bytes.len(), grown.len(), "the file keeps its size");
    assert_eq!(find(&bytes, &[b"mdat"]).unwrap().start, mdat.start, "the media data stays put");
    assert_rewrite(&before, &before_bytes, &work.path, &tags, &visuals);

    // The padding absorbed the difference.
    let free = find(&bytes, &[b"free"]).unwrap();
    assert!(free.end - free.start > 4096);
}

#[test]
fn growing_without_padding_leaves_no_free_box() {
    let work = Work::new("mp4-aac-faststart.m4a", "nopadding");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    write_with(&work.path, &rich_tags(), &[], 0);
    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &[]);
    assert_eq!(kinds(&work.bytes()), ["ftyp", "moov", "mdat"]);
}

#[test]
fn moov_at_the_end_grows_without_moving_the_audio() {
    let work = Work::new("mp4-aac-moov-end.m4a", "moovend");
    let before = probe(&work.path);
    let before_bytes = work.bytes();
    assert_eq!(kinds(&before_bytes), ["ftyp", "free", "mdat", "moov"]);

    let visuals = [visual(png()), visual(jpeg())];
    write(&work.path, &rich_tags(), &visuals);
    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &visuals);

    let bytes = work.bytes();
    assert_eq!(kinds(&bytes), ["ftyp", "free", "mdat", "moov"], "no padding after the last box");

    // Everything in front of the moov is untouched.
    let moov = find(&bytes, &[b"moov"]).unwrap();
    let old_moov = find(&before_bytes, &[b"moov"]).unwrap();
    assert_eq!(moov.start, old_moov.start);
    assert_eq!(bytes[..moov.start], before_bytes[..old_moov.start]);

    // The stco did not need to move.
    let stco = |b: &[u8]| {
        let stco = find(b, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stco"]).unwrap();
        slice(b, stco).to_vec()
    };
    assert_eq!(stco(&bytes), stco(&before_bytes));
}

#[test]
fn moov_at_the_end_shrinks_with_the_file() {
    let work = Work::new("mp4-pretagged-moov-end.m4a", "moovshrink");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    let tags = vec![tag("TITLE", StandardTag::TrackTitle(s("Only a title")))];
    write(&work.path, &tags, &[]);

    let bytes = work.bytes();
    assert!(bytes.len() < before_bytes.len() - 2000);
    assert_eq!(kinds(&bytes), kinds(&before_bytes));

    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&tags));
    assert!(after.visuals.is_empty());
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);
}

#[test]
fn alac_round_trips() {
    let work = Work::new("mp4-alac.m4a", "alac");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    let visuals = [visual(jpeg())];
    write(&work.path, &rich_tags(), &visuals);
    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &visuals);
}

#[test]
fn every_chunk_of_every_track_is_moved() {
    let work = Work::new("mp4-aac-two-tracks.m4a", "twotracks");
    let before = probe(&work.path);
    let before_bytes = work.bytes();
    assert_eq!(before.pcm.len(), 2);

    let visuals = [visual(jpeg()), visual(png())];
    write(&work.path, &rich_tags(), &visuals);
    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &visuals);

    // Both tracks' chunk tables moved by the same amount.
    let bytes = work.bytes();
    let first_chunk = |b: &[u8], trak: usize| {
        let moov = find(b, &[b"moov"]).unwrap();
        let traks: Vec<B> = boxes(b, moov.start + moov.header, moov.end)
            .into_iter()
            .filter(|b| &b.kind == b"trak")
            .collect();
        let mdia = child(b, traks[trak], b"mdia").unwrap();
        let minf = child(b, mdia, b"minf").unwrap();
        let stbl = child(b, minf, b"stbl").unwrap();
        let stco = child(b, stbl, b"stco").unwrap();
        u32::from_be_bytes(b[stco.start + 16..stco.start + 20].try_into().unwrap())
    };
    let delta0 = first_chunk(&bytes, 0) - first_chunk(&before_bytes, 0);
    let delta1 = first_chunk(&bytes, 1) - first_chunk(&before_bytes, 1);
    assert!(delta0 > 0);
    assert_eq!(delta0, delta1);
}

#[test]
fn fragmented_file_moves_its_fragments_and_their_index() {
    let work = Work::new("mp4-aac-fragmented.m4a", "fragmented");
    let before = probe(&work.path);
    let before_bytes = work.bytes();
    assert_eq!(kinds(&before_bytes), ["ftyp", "moov", "moof", "mdat", "mfra"]);

    let visuals = [visual(jpeg())];
    write(&work.path, &rich_tags(), &visuals);
    assert_rewrite(&before, &before_bytes, &work.path, &rich_tags(), &visuals);

    let bytes = work.bytes();
    assert_eq!(kinds(&bytes), ["ftyp", "moov", "free", "moof", "mdat", "mfra"]);

    // The random access table points at the fragments' new positions.
    let moof = find(&bytes, &[b"moof"]).unwrap();
    assert!(moof.start > find(&before_bytes, &[b"moof"]).unwrap().start);

    let old_moof = find(&before_bytes, &[b"moof"]).unwrap();
    assert_eq!(tfra_offsets(&before_bytes), [old_moof.start]);
    assert_eq!(tfra_offsets(&bytes), [moof.start]);

    // Writing again, now with something that fits, keeps the fragments where they are.
    let mut tags = rich_tags();
    tags.truncate(10);
    write(&work.path, &tags, &[]);
    assert_eq!(find(&work.bytes(), &[b"moof"]).unwrap().start, moof.start);
    let again = probe(&work.path);
    assert!(again.packets == before.packets);
}

// ---------------------------------------------------------------------------------------------
// Pre-tagged files
// ---------------------------------------------------------------------------------------------

#[test]
fn a_file_written_with_its_own_tags_reads_the_same() {
    for name in ["mp4-pretagged.m4a", "mp4-pretagged-moov-end.m4a"] {
        let work = Work::new(name, "own");
        let before = probe(&work.path);
        let before_bytes = work.bytes();

        assert!(before.tags.len() > 30, "the fixture is richly tagged");
        assert_eq!(before.visuals.len(), 2);

        write(&work.path, &before.tags, &before.visuals);

        let after = probe(&work.path);
        assert_eq!(everything(&after.tags), everything(&before.tags), "{name}");
        assert_eq!(visual_data(&after.visuals), visual_data(&before.visuals), "{name}");
        assert!(after.packets == before.packets, "{name}");
        assert_eq!(after.pcm, before.pcm, "{name}");

        // The atoms that are not tags came through, byte for byte.
        let old_atoms = ilst_atoms(&before_bytes);
        let new_atoms = ilst_atoms(&work.bytes());

        for kind in [b"xxxx", b"zzzz"] {
            let old = old_atoms.iter().find(|(k, _)| k == kind).expect("in the fixture");
            let new = new_atoms.iter().find(|(k, _)| k == kind).expect("preserved");
            assert_eq!(old.1, new.1, "{name}: {}", String::from_utf8_lossy(kind));
        }

        // The mdir handler and the other children of the user data are still there.
        assert!(find(&work.bytes(), &[b"moov", b"udta", b"meta", b"hdlr"]).is_some());
    }
}

#[test]
fn editing_a_pretagged_file_replaces_exactly_the_tags_given() {
    let work = Work::new("mp4-pretagged.m4a", "edit");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    // Keep everything but the artists and the sort names, and change the title.
    let mut tags: Vec<Tag> = before
        .tags
        .iter()
        .filter(|t| {
            !matches!(
                t.std,
                Some(StandardTag::Artist(_))
                    | Some(StandardTag::SortArtist(_))
                    | Some(StandardTag::SortTrackTitle(_))
                    | Some(StandardTag::TrackTitle(_))
            )
        })
        .cloned()
        .collect();
    tags.push(tag("TITLE", StandardTag::TrackTitle(s("Edited title"))));
    tags.push(tag("ARTIST", StandardTag::Artist(s("Only Artist"))));

    write(&work.path, &tags, &before.visuals[..1]);

    let after = probe(&work.path);
    let titles: Vec<String> = after
        .tags
        .iter()
        .filter_map(|t| match &t.std {
            Some(StandardTag::TrackTitle(v)) => Some(v.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["Edited title"]);

    let artists: Vec<String> = after
        .tags
        .iter()
        .filter_map(|t| match &t.std {
            Some(StandardTag::Artist(v)) => Some(v.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(artists, ["Only Artist"]);

    assert!(!after.tags.iter().any(|t| matches!(t.std, Some(StandardTag::SortArtist(_)))));
    assert_eq!(visual_data(&after.visuals), visual_data(&before.visuals[..1]));

    // The rest is as it was.
    let keep = |tags: &[Tag]| {
        everything(
            &tags
                .iter()
                .filter(|t| {
                    !matches!(
                        t.std,
                        Some(StandardTag::Artist(_))
                            | Some(StandardTag::SortArtist(_))
                            | Some(StandardTag::SortTrackTitle(_))
                            | Some(StandardTag::TrackTitle(_))
                    )
                })
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(keep(&after.tags), keep(&before.tags));

    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);

    // The unmanaged atoms are still there, and the removed ones are not.
    let kinds: Vec<[u8; 4]> = ilst_atoms(&work.bytes()).into_iter().map(|(k, _)| k).collect();
    assert!(kinds.contains(b"xxxx") && kinds.contains(b"zzzz"));
    assert!(!kinds.contains(b"soar") && !kinds.contains(b"sonm"));
    let _ = before_bytes;
}

#[test]
fn shrinking_moves_the_chunk_offsets_back() {
    let work = Work::new("mp4-pretagged.m4a", "shrink");
    let before = probe(&work.path);
    let before_bytes = work.bytes();

    let tags = vec![tag("TITLE", StandardTag::TrackTitle(s("Just a title")))];
    write_with(&work.path, &tags, &[], 0);

    let bytes = work.bytes();
    assert!(bytes.len() < before_bytes.len() - 2500, "the file shrank");
    assert_eq!(kinds(&bytes), ["ftyp", "moov", "mdat"]);

    let mdat_before = find(&before_bytes, &[b"mdat"]).unwrap();
    let mdat = find(&bytes, &[b"mdat"]).unwrap();
    assert!(mdat.start < mdat_before.start);

    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&tags));
    assert!(after.visuals.is_empty());
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);

    // Growing it again, with padding asked for, leaves that padding behind the moov.
    let tags = vec![tag("TITLE", StandardTag::TrackTitle(s("Just a title, but a longer one")))];
    write_with(&work.path, &tags, &[], 1000);
    let padded = probe(&work.path);
    assert!(padded.packets == before.packets);
    assert_eq!(padded.pcm, before.pcm);
    assert_eq!(kinds(&work.bytes()), ["ftyp", "moov", "free", "mdat"]);
    let free = find(&work.bytes(), &[b"free"]).unwrap();
    assert_eq!(free.end - free.start, 1000);
}

#[test]
fn empty_tags_remove_the_metadata_boxes() {
    // The fixture has an empty ilst, written by ffmpeg.
    let work = Work::new("mp4-aac-faststart.m4a", "empty");
    let before = probe(&work.path);
    let before_bytes = work.bytes();
    assert!(find(&before_bytes, &[b"moov", b"udta"]).is_some());

    write(&work.path, &[], &[]);

    let bytes = work.bytes();
    assert!(find(&bytes, &[b"moov", b"udta"]).is_none(), "no empty udta is left");

    let after = probe(&work.path);
    assert!(after.tags.is_empty() && after.visuals.is_empty());
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);

    // The room the udta left is a free box, so the audio did not move.
    assert_eq!(
        find(&bytes, &[b"mdat"]).unwrap().start,
        find(&before_bytes, &[b"mdat"]).unwrap().start
    );

    // Writing nothing to a file with unmanaged atoms keeps them.
    let work = Work::new("mp4-pretagged.m4a", "empty-unmanaged");
    write(&work.path, &[], &[]);
    let after = probe(&work.path);
    assert!(after.tags.is_empty() && after.visuals.is_empty());
    let mut kinds: Vec<[u8; 4]> = ilst_atoms(&work.bytes()).into_iter().map(|(k, _)| k).collect();
    // They keep the order they were in.
    assert_eq!(kinds, [*b"zzzz", *b"xxxx"]);
    kinds.sort();
    assert_eq!(kinds, [*b"xxxx", *b"zzzz"]);
}

// ---------------------------------------------------------------------------------------------
// Box variants
// ---------------------------------------------------------------------------------------------

/// Rewrites the file `ftyp moov free mdat` as `ftyp moov mdat` with a header of 8 bytes more on
/// `mdat`: the audio stays where it was.
fn with_wide_mdat_header(bytes: &[u8]) -> Vec<u8> {
    let boxes = top(bytes);
    let free = boxes.iter().find(|b| &b.kind == b"free").unwrap();
    let mdat = boxes.iter().find(|b| &b.kind == b"mdat").unwrap();
    assert_eq!(free.end, mdat.start);
    assert_eq!(free.end - free.start, 8);

    let mut out = bytes[..free.start].to_vec();
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&((mdat.end - mdat.start + 8) as u64).to_be_bytes());
    out.extend_from_slice(&bytes[mdat.start + 8..mdat.end]);
    out
}

#[test]
fn a_64_bit_mdat_header_is_kept() {
    let work = Work::new("mp4-aac-faststart.m4a", "wide-mdat");
    let bytes = with_wide_mdat_header(&work.bytes());
    work.set_bytes(&bytes);

    let before = probe(&work.path);
    write(&work.path, &rich_tags(), &[visual(jpeg())]);

    let after_bytes = work.bytes();
    let mdat = find(&after_bytes, &[b"mdat"]).unwrap();
    assert_eq!(mdat.header, 16);
    assert_eq!(&after_bytes[mdat.start..mdat.start + 4], &1u32.to_be_bytes());

    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&rich_tags()));
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);
}

#[test]
fn a_mdat_that_extends_to_the_end_of_the_file_is_kept() {
    let work = Work::new("mp4-aac-faststart.m4a", "open-mdat");
    let before = probe(&work.path);
    let original = work.bytes();
    let old_mdat = find(&original, &[b"mdat"]).unwrap();

    let mut bytes = original.clone();
    bytes[old_mdat.start..old_mdat.start + 4].copy_from_slice(&0u32.to_be_bytes());
    work.set_bytes(&bytes);

    write(&work.path, &rich_tags(), &[]);

    let mut after_bytes = work.bytes();
    let mdat = find(&after_bytes, &[b"mdat"]).unwrap();
    assert_eq!(&after_bytes[mdat.start..mdat.start + 4], &0u32.to_be_bytes());
    assert_eq!(mdat.end, after_bytes.len());
    assert_eq!(after_bytes[mdat.start + 8..], original[old_mdat.start + 8..]);

    // Give the mdat its size back: the file reads as the original did.
    let size = (mdat.end - mdat.start) as u32;
    after_bytes[mdat.start..mdat.start + 4].copy_from_slice(&size.to_be_bytes());
    work.set_bytes(&after_bytes);

    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&rich_tags()));
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);
}

#[test]
fn a_64_bit_moov_header_is_read_and_a_moov_that_extends_to_the_end_too() {
    // 64-bit moov: `ftyp moov(+8) mdat`, the free box is gone, the audio stays where it was.
    let work = Work::new("mp4-aac-faststart.m4a", "wide-moov");
    let before = probe(&work.path);
    let bytes = work.bytes();
    let boxes = top(&bytes);
    let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
    let free = boxes.iter().find(|b| &b.kind == b"free").unwrap();

    let mut wide = bytes[..moov.start].to_vec();
    wide.extend_from_slice(&1u32.to_be_bytes());
    wide.extend_from_slice(b"moov");
    wide.extend_from_slice(&((moov.end - moov.start + 8) as u64).to_be_bytes());
    wide.extend_from_slice(&bytes[moov.start + 8..moov.end]);
    wide.extend_from_slice(&bytes[free.end..]);
    work.set_bytes(&wide);

    write(&work.path, &rich_tags(), &[]);
    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&rich_tags()));
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);

    // A moov that extends to the end of the file.
    let work = Work::new("mp4-aac-moov-end.m4a", "open-moov");
    let before = probe(&work.path);
    let mut bytes = work.bytes();
    let moov = find(&bytes, &[b"moov"]).unwrap();
    bytes[moov.start..moov.start + 4].copy_from_slice(&0u32.to_be_bytes());
    work.set_bytes(&bytes);

    write(&work.path, &rich_tags(), &[visual(png())]);
    let after = probe(&work.path);
    assert_eq!(std_tags(&after.tags), std_tags(&rich_tags()));
    assert!(after.packets == before.packets);
    assert_eq!(after.pcm, before.pcm);
    assert_eq!(kinds(&work.bytes()), ["ftyp", "free", "mdat", "moov"]);
}

// ---------------------------------------------------------------------------------------------
// Tag copies the reader would also see
// ---------------------------------------------------------------------------------------------

/// A minimal ID3v2.3 tag with text frames.
fn id3v23(frames: &[(&[u8; 4], &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, text) in frames {
        body.extend_from_slice(*id);
        body.extend_from_slice(&((text.len() + 1) as u32).to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.push(0); // ISO-8859-1.
        body.extend_from_slice(text.as_bytes());
    }

    let size = body.len();
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend_from_slice(&[
        ((size >> 21) & 0x7f) as u8,
        ((size >> 14) & 0x7f) as u8,
        ((size >> 7) & 0x7f) as u8,
        (size & 0x7f) as u8,
    ]);
    tag.extend_from_slice(&body);
    tag
}

fn full_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn id32_box() -> Vec<u8> {
    let tag = id3v23(&[(b"TIT2", "Old ID32 title"), (b"TALB", "Old ID32 album")]);

    // Version and flags, packed language ("und"), then the tag.
    let mut body = vec![0, 0, 0, 0, 0x55, 0xc4];
    body.extend_from_slice(&tag);
    full_box(b"ID32", &body)
}

fn titles(probed: &Probed) -> Vec<String> {
    probed
        .tags
        .iter()
        .filter_map(|t| match &t.std {
            Some(StandardTag::TrackTitle(v)) => Some(v.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_top_level_id32_meta_box_is_removed() {
    let work = Work::new("mp4-aac-moov-end.m4a", "top-id32");

    // `hdlr` with the ID32 handler, then the ID32 box.
    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"ID32");
    hdlr_body.extend_from_slice(&[0; 13]);
    let mut meta_body = vec![0u8; 4];
    meta_body.extend_from_slice(&full_box(b"hdlr", &hdlr_body));
    meta_body.extend_from_slice(&id32_box());

    // Something follows, so that the tag is not mistaken for one appended to the file.
    let mut bytes = work.bytes();
    bytes.extend_from_slice(&full_box(b"meta", &meta_body));
    bytes.extend_from_slice(&full_box(b"free", &[0u8; 32]));
    work.set_bytes(&bytes);

    let before = probe(&work.path);
    assert_eq!(titles(&before), ["Old ID32 title"], "the ID32 tags are read");

    let tags = vec![tag("TITLE", StandardTag::TrackTitle(s("New title")))];
    write(&work.path, &tags, &[]);

    let after = probe(&work.path);
    assert_eq!(titles(&after), ["New title"]);
    assert_eq!(after.tags.len(), 1);
    // The meta box is gone. The moov grew, so padding follows it, then the free box that was there.
    assert_eq!(kinds(&work.bytes()), ["ftyp", "free", "mdat", "moov", "free", "free"]);
    assert!(after.packets == before.packets);
}

#[test]
fn an_id32_box_in_the_user_data_is_removed() {
    let work = Work::new("mp4-aac-moov-end.m4a", "udta-id32");
    let mut bytes = work.bytes();

    // The moov is the last box, and the udta its last child.
    let moov = find(&bytes, &[b"moov"]).unwrap();
    let udta = find(&bytes, &[b"moov", b"udta"]).unwrap();
    assert_eq!(moov.end, bytes.len());
    assert_eq!(udta.end, bytes.len());

    let id32 = id32_box();
    for b in [moov, udta] {
        let size = (b.end - b.start + id32.len()) as u32;
        bytes[b.start..b.start + 4].copy_from_slice(&size.to_be_bytes());
    }
    bytes.extend_from_slice(&id32);
    // Something follows, so that the tag is not mistaken for one appended to the file.
    bytes.extend_from_slice(&full_box(b"free", &[0u8; 32]));
    work.set_bytes(&bytes);

    let before = probe(&work.path);
    assert_eq!(titles(&before), ["Old ID32 title"]);

    let tags = vec![tag("TITLE", StandardTag::TrackTitle(s("New title")))];
    write(&work.path, &tags, &[]);

    let after = probe(&work.path);
    assert_eq!(titles(&after), ["New title"]);
    assert_eq!(after.tags.len(), 1);
    assert!(!work.bytes().windows(4).any(|w| w == b"ID32"));
}

// ---------------------------------------------------------------------------------------------
// Failures leave the file alone
// ---------------------------------------------------------------------------------------------

fn assert_untouched(work: &Work, original: &[u8]) {
    assert_eq!(work.bytes(), original, "the file must not change");

    let leftovers: Vec<_> = fs::read_dir(&work.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temporary file is left: {leftovers:?}");
}

#[test]
fn fragments_with_an_explicit_base_data_offset_are_unsupported() {
    let work = Work::new("mp4-aac-fragmented.m4a", "explicit-base");
    let mut bytes = work.bytes();

    // Set the `base-data-offset-present` flag of the tfhd.
    let tfhd = find(&bytes, &[b"moof"]).and_then(|moof| {
        let traf = child(&bytes, moof, b"traf")?;
        child(&bytes, traf, b"tfhd")
    });
    let tfhd = tfhd.unwrap();
    bytes[tfhd.start + 8 + 3] |= 1;
    work.set_bytes(&bytes);

    let err = write_file(&work.path, &rich_tags(), &[], &WriteOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    assert_untouched(&work, &bytes);
}

#[test]
fn a_file_without_a_moov_is_malformed() {
    let work = Work::new("mp4-aac-faststart.m4a", "no-moov");
    let bytes = work.bytes();
    let boxes = top(&bytes);

    let mut cut = Vec::new();
    for b in boxes.iter().filter(|b| &b.kind != b"moov") {
        cut.extend_from_slice(slice(&bytes, *b));
    }
    work.set_bytes(&cut);

    let err = write_file(&work.path, &[], &[], &WriteOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Malformed(_)), "{err}");
    assert_untouched(&work, &cut);
}

#[test]
fn a_foreign_metadata_handler_is_unsupported() {
    let work = Work::new("mp4-pretagged-moov-end.m4a", "mdta");
    let mut bytes = work.bytes();

    let hdlr = find(&bytes, &[b"moov", b"udta", b"meta", b"hdlr"]).unwrap();
    let at = hdlr.start + 8 + 8;
    assert_eq!(&bytes[at..at + 4], b"mdir");
    bytes[at..at + 4].copy_from_slice(b"mdta");
    work.set_bytes(&bytes);

    let err = write_file(&work.path, &rich_tags(), &[], &WriteOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    assert_untouched(&work, &bytes);
}

#[test]
fn cover_art_that_is_not_jpeg_png_or_bmp_is_unsupported() {
    let work = Work::new("mp4-aac-faststart.m4a", "gif");
    let bytes = work.bytes();

    let gif = visual(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec());
    let err = write_file(&work.path, &[], &[gif], &WriteOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    assert_untouched(&work, &bytes);
}

#[test]
fn the_original_value_of_unmapped_raw_tags_survives() {
    // A tag with no standard tag and a value that is not text.
    let work = Work::new("mp4-aac-faststart.m4a", "raw-values");
    let tags = vec![
        Tag::new(RawTag::new("pgap", RawValue::Boolean(true))),
        Tag::new(RawTag::new("apID", "me@example.com")),
        Tag::new(RawTag::new("BLOB", RawValue::from(&[1u8, 2, 3][..]))),
    ];

    write(&work.path, &tags, &[]);
    let after = probe(&work.path);

    let find =
        |key: &str| after.tags.iter().find(|t| t.raw.key == key).map(|t| t.raw.value.clone());
    assert_eq!(find("pgap"), Some(RawValue::SignedInt(1)));
    assert_eq!(find("apID"), Some(RawValue::from("me@example.com")));
    assert_eq!(find("com.apple.iTunes:BLOB"), Some(RawValue::from(&[1u8, 2, 3][..])));
}
