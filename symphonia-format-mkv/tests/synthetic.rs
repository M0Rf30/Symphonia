// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression tests for the Matroska demuxer using small synthetic files.

use std::io::Cursor;

use symphonia_core::formats::FormatReader;
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::{StandardTag, StandardVisualKey};
use symphonia_core::units::Time;
use symphonia_format_mkv::MkvReader;

// EBML element writing helpers.

/// Encode the size of an element's data.
fn size(n: usize) -> Vec<u8> {
    if n < 0x7f {
        vec![0x80 | n as u8]
    }
    else if n < 0x3fff {
        vec![0x40 | (n >> 8) as u8, n as u8]
    }
    else if n < 0x1f_ffff {
        vec![0x20 | (n >> 16) as u8, (n >> 8) as u8, n as u8]
    }
    else {
        vec![0x10 | (n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    }
}

/// The encoded data size that indicates an unknown size.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    [id, &size(payload.len()), payload].concat()
}

fn uint(id: &[u8], value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
    el(id, &bytes[skip..])
}

/// An unsigned integer element with a fixed encoded size.
fn uint32(id: &[u8], value: u32) -> Vec<u8> {
    el(id, &value.to_be_bytes())
}

fn sint(id: &[u8], value: i64) -> Vec<u8> {
    el(id, &value.to_be_bytes())
}

fn float(id: &[u8], value: f64) -> Vec<u8> {
    el(id, &value.to_be_bytes())
}

fn string(id: &[u8], value: &str) -> Vec<u8> {
    el(id, value.as_bytes())
}

// Element IDs.
const ID_EBML: &[u8] = &[0x1a, 0x45, 0xdf, 0xa3];
const ID_SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const ID_INFO: &[u8] = &[0x15, 0x49, 0xa9, 0x66];
const ID_TRACKS: &[u8] = &[0x16, 0x54, 0xae, 0x6b];
const ID_CLUSTER: &[u8] = &[0x1f, 0x43, 0xb6, 0x75];
const ID_CUES: &[u8] = &[0x1c, 0x53, 0xbb, 0x6b];
const ID_CHAPTERS: &[u8] = &[0x10, 0x43, 0xa7, 0x70];
const ID_ATTACHMENTS: &[u8] = &[0x19, 0x41, 0xa4, 0x69];
const ID_TAGS: &[u8] = &[0x12, 0x54, 0xc3, 0x67];

fn ebml_header() -> Vec<u8> {
    el(
        ID_EBML,
        &[
            uint(&[0x42, 0x86], 1),
            uint(&[0x42, 0xf7], 1),
            uint(&[0x42, 0xf2], 4),
            uint(&[0x42, 0xf3], 8),
            string(&[0x42, 0x82], "matroska"),
            uint(&[0x42, 0x87], 4),
            uint(&[0x42, 0x85], 2),
        ]
        .concat(),
    )
}

fn info() -> Vec<u8> {
    el(
        ID_INFO,
        &[
            uint(&[0x2a, 0xd7, 0xb1], 1_000_000),
            string(&[0x4d, 0x80], "test"),
            string(&[0x57, 0x41], "test"),
        ]
        .concat(),
    )
}

/// A single audio track.
struct Track {
    codec: &'static str,
    sample_rate: f64,
    channels: u64,
    bit_depth: Option<u64>,
    /// `CodecDelay` in nanoseconds.
    codec_delay: u64,
    /// `SeekPreRoll` in nanoseconds.
    seek_pre_roll: u64,
    /// `DefaultDuration` in nanoseconds.
    default_duration: Option<u64>,
}

impl Track {
    /// 16-bit mono PCM at 48kHz.
    fn pcm() -> Self {
        Track {
            codec: "A_PCM/INT/LIT",
            sample_rate: 48000.0,
            channels: 1,
            bit_depth: Some(16),
            codec_delay: 0,
            seek_pre_roll: 0,
            default_duration: None,
        }
    }

    /// Opus with a 312 sample (6.5ms) pre-skip and a default duration of 20ms.
    fn opus() -> Self {
        Track {
            codec: "A_OPUS",
            sample_rate: 48000.0,
            channels: 2,
            bit_depth: None,
            codec_delay: 6_500_000,
            seek_pre_roll: 80_000_000,
            default_duration: Some(20_000_000),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut audio = vec![float(&[0xb5], self.sample_rate), uint(&[0x9f], self.channels)];

        if let Some(bits) = self.bit_depth {
            audio.push(uint(&[0x62, 0x64], bits));
        }

        let mut entry = vec![
            uint(&[0xd7], 1),
            uint(&[0x73, 0xc5], 1),
            uint(&[0x83], 2),
            string(&[0x86], self.codec),
            el(&[0xe1], &audio.concat()),
        ];

        if self.codec_delay > 0 {
            entry.push(uint(&[0x56, 0xaa], self.codec_delay));
        }
        if self.seek_pre_roll > 0 {
            entry.push(uint(&[0x56, 0xbb], self.seek_pre_roll));
        }
        if let Some(dur) = self.default_duration {
            entry.push(uint(&[0x23, 0xe3, 0x83], dur));
        }

        el(ID_TRACKS, &el(&[0xae], &entry.concat()))
    }
}

/// A block (of track 1).
struct Block {
    /// The absolute timestamp of the block in milliseconds.
    ts: i64,
    /// The length of the block's data. The first byte of the data is the index of the block.
    len: usize,
    /// The duration of the block in milliseconds. Forces a block group.
    duration: Option<u64>,
    /// The discard padding of the block in nanoseconds. Forces a block group.
    padding: Option<i64>,
}

impl Block {
    fn new(ts: i64, len: usize) -> Self {
        Block { ts, len, duration: None, padding: None }
    }
}

/// A cue point.
struct Cue {
    /// The time of the cue in milliseconds.
    time: u64,
    cluster: usize,
    /// The index of the block in the cluster the cue refers to.
    block: usize,
}

#[derive(Default)]
struct File {
    /// Elements before the cues and clusters (in addition to info and tracks).
    head: Vec<Vec<u8>>,
    /// The clusters: the timestamp of the cluster, and the blocks in it.
    clusters: Vec<(i64, Vec<Block>)>,
    cues: Vec<Cue>,
    /// If the segment and clusters have an unknown size.
    live: bool,
}

impl File {
    fn to_bytes(&self, track: &Track) -> Vec<u8> {
        let mut head = [info(), track.to_bytes()].concat();
        head.extend(self.head.concat());

        // Build the clusters. Track the offset of each block within its cluster.
        let mut index = 0u8;
        let mut clusters = Vec::new();
        let mut block_offsets = Vec::new();

        for (cluster_ts, blocks) in &self.clusters {
            let mut data = uint(&[0xe7], *cluster_ts as u64);
            let mut offsets = Vec::new();

            for blk in blocks {
                offsets.push(data.len());

                let rel = (blk.ts - cluster_ts) as i16;
                let mut payload = vec![0x81, (rel >> 8) as u8, rel as u8];

                if blk.duration.is_some() || blk.padding.is_some() {
                    payload.push(0x00);
                    payload.extend(vec![index; blk.len]);

                    let mut group = vec![el(&[0xa1], &payload)];
                    if let Some(dur) = blk.duration {
                        group.push(uint(&[0x9b], dur));
                    }
                    if let Some(padding) = blk.padding {
                        group.push(sint(&[0x75, 0xa2], padding));
                    }
                    data.extend(el(&[0xa0], &group.concat()));
                }
                else {
                    payload.push(0x80);
                    payload.extend(vec![index; blk.len]);
                    data.extend(el(&[0xa3], &payload));
                }

                index = index.wrapping_add(1);
            }

            let mut cluster = ID_CLUSTER.to_vec();
            if self.live {
                cluster.extend(UNKNOWN_SIZE);
                cluster.extend(&data);
            }
            else {
                cluster.extend(size(data.len()));
                cluster.extend(&data);
            }

            clusters.push(cluster);
            block_offsets.push(offsets);
        }

        // Build the cues. They are placed before the clusters so that the demuxer finds them
        // without a seek head.
        let make_cues = |base: usize| -> Vec<u8> {
            if self.cues.is_empty() {
                return Vec::new();
            }

            let mut points = Vec::new();
            let mut cluster_pos = Vec::new();
            let mut pos = base;
            for cluster in &clusters {
                cluster_pos.push(pos);
                pos += cluster.len();
            }

            for cue in &self.cues {
                let positions = el(
                    &[0xb7],
                    &[
                        uint(&[0xf7], 1),
                        uint32(&[0xf1], cluster_pos[cue.cluster] as u32),
                        uint32(&[0xf0], block_offsets[cue.cluster][cue.block] as u32),
                    ]
                    .concat(),
                );
                points.push(el(&[0xbb], &[uint32(&[0xb3], cue.time as u32), positions].concat()));
            }

            el(ID_CUES, &points.concat())
        };

        let cues_len = make_cues(0).len();
        let cues = make_cues(head.len() + cues_len);

        let payload = [head, cues, clusters.concat()].concat();

        let mut segment = ID_SEGMENT.to_vec();
        if self.live {
            segment.extend(UNKNOWN_SIZE);
        }
        else {
            segment.extend(size_8(payload.len()));
        }
        segment.extend(payload);

        [ebml_header(), segment].concat()
    }
}

/// Encode a size using 8 bytes.
fn size_8(n: usize) -> Vec<u8> {
    let mut bytes = (n as u64).to_be_bytes().to_vec();
    bytes[0] = 0x01;
    bytes
}

fn open(data: Vec<u8>) -> MkvReader<'static> {
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default());
    MkvReader::try_new(mss, FormatOptions::default()).expect("file should open")
}

/// Read all remaining packets. Returns the packets and the result of the last call.
fn read_all(reader: &mut MkvReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    loop {
        match reader.next_packet().expect("reading should not fail") {
            Some(packet) => packets.push(packet),
            None => return packets,
        }
    }
}

/// A PCM file of 100 blocks of 10ms in 4 clusters.
fn pcm_file() -> File {
    let mut file = File::default();

    for cluster in 0..4 {
        let cluster_ts = cluster * 250;
        let blocks = (0..25).map(|i| Block::new(cluster_ts + i * 10, 960)).collect();
        file.clusters.push((cluster_ts, blocks));
    }

    file
}

fn seek_time(
    reader: &mut MkvReader<'_>,
    millis: u32,
) -> Result<SeekedTo, symphonia_core::errors::Error> {
    reader.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time: Time::try_from_secs_f64(f64::from(millis) / 1000.0).unwrap(),
            track_id: None,
        },
    )
}

#[test]
fn audio_track_timebase_is_the_sample_rate() {
    let mut reader = open(pcm_file().to_bytes(&Track::pcm()));

    assert_eq!(
        reader.tracks()[0].time_base.map(|tb| (tb.numer.get(), tb.denom.get())),
        Some((1, 48000))
    );

    // The duration of PCM blocks is derived from their size. All packets are contiguous.
    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 100);

    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.pts.get(), 480 * i as i64);
        assert_eq!(packet.dur.get(), 480);
        assert_eq!(packet.trim_start.get(), 0);
        assert_eq!(packet.trim_end.get(), 0);
    }
}

#[test]
fn seek_after_reaching_the_end_of_the_stream() {
    let mut reader = open(pcm_file().to_bytes(&Track::pcm()));

    assert_eq!(read_all(&mut reader).len(), 100);
    // The end of the stream is reported consistently.
    assert!(reader.next_packet().unwrap().is_none());

    // Seeking back works, no matter how often.
    for _ in 0..2 {
        let seeked = seek_time(&mut reader, 500).unwrap();
        assert_eq!(seeked.actual_ts.get(), 24000);
        assert_eq!(seeked.required_ts.get(), 24000);

        let packets = read_all(&mut reader);
        assert_eq!(packets.len(), 50);
        assert_eq!(packets[0].data[0], 50);
    }

    // A seek past the end fails but leaves the reader usable.
    assert!(seek_time(&mut reader, 5000).is_err());
    assert!(reader.next_packet().unwrap().is_none());

    let seeked = seek_time(&mut reader, 0).unwrap();
    assert_eq!(seeked.actual_ts.get(), 0);
    assert_eq!(read_all(&mut reader).len(), 100);
}

#[test]
fn seek_without_cues_goes_backwards() {
    let mut reader = open(pcm_file().to_bytes(&Track::pcm()));

    let seeked = seek_time(&mut reader, 620).unwrap();
    assert_eq!(seeked.actual_ts.get(), 29760);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 62);

    // Backwards.
    let seeked = seek_time(&mut reader, 105).unwrap();
    assert_eq!(seeked.actual_ts.get(), 4800);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 10);
}

#[test]
fn seek_with_cues_uses_the_timestamp_of_the_cluster() {
    let mut file = pcm_file();

    // Cue points refer to blocks in the middle of clusters, so that their timestamps are not the
    // timestamps of the clusters.
    file.cues = vec![
        Cue { time: 120, cluster: 0, block: 12 },
        Cue { time: 250, cluster: 1, block: 0 },
        Cue { time: 370, cluster: 1, block: 12 },
    ];

    let mut reader = open(file.to_bytes(&Track::pcm()));

    let seeked = seek_time(&mut reader, 385).unwrap();
    assert_eq!(seeked.required_ts.get(), 385 * 48);
    assert_eq!(seeked.actual_ts.get(), 380 * 48);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 38);

    // Before the first cue.
    let seeked = seek_time(&mut reader, 50).unwrap();
    assert_eq!(seeked.actual_ts.get(), 50 * 48);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 5);

    // After the last.
    let seeked = seek_time(&mut reader, 995).unwrap();
    assert_eq!(seeked.actual_ts.get(), 990 * 48);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 99);
}

/// Six 20ms Opus packets (the first with a 6.5ms pre-skip, and the last with 13.5ms of padding), as
/// written by ffmpeg.
fn opus_file() -> File {
    let mut file = File::default();

    let mut blocks: Vec<Block> =
        [0, 21, 41, 61, 81].iter().map(|&ts| Block::new(ts, 100)).collect();

    blocks.push(Block { ts: 101, len: 100, duration: Some(7), padding: Some(13_500_000) });

    file.clusters.push((0, blocks));
    file
}

#[test]
fn opus_delay_and_discard_padding_are_trimmed() {
    let mut reader = open(opus_file().to_bytes(&Track::opus()));

    assert_eq!(reader.tracks()[0].delay, Some(312));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 6);

    // Packet durations, trims, and timestamps are in samples.
    let first = &packets[0];
    assert_eq!((first.pts.get(), first.dur.get(), first.trim_start.get()), (-312, 648, 312));

    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.pts.get(), -312 + 960 * i as i64);
        assert_eq!(packet.block_dur().get(), 960);
    }

    let last = &packets[5];
    assert_eq!((last.dur.get(), last.trim_start.get(), last.trim_end.get()), (312, 0, 648));

    // 6 packets of 960 frames, less the pre-skip and the padding.
    let valid: u64 = packets.iter().map(|packet| packet.dur.get()).sum();
    assert_eq!(valid, 6 * 960 - 312 - 648);

    // Seeking back to the start trims the pre-skip again.
    seek_time(&mut reader, 0).unwrap();
    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!((first.pts.get(), first.trim_start.get()), (-312, 312));
}

#[test]
fn opus_seek_backs_off_by_the_seek_pre_roll() {
    let mut file = File::default();

    // 100 packets of 20ms with exact timestamps.
    file.clusters.push((0, (0..100).map(|i| Block::new(i * 20, 100)).collect()));

    let mut reader = open(file.to_bytes(&Track::opus()));

    // 1s is 48000 samples. The pre-skip is 312 samples, and the pre-roll is 3840 samples (80ms).
    let seeked = seek_time(&mut reader, 1000).unwrap();
    assert_eq!(seeked.required_ts.get(), 48000);
    // The last packet that starts before 48000 - 3840 = 44160 (the packet that starts at
    // 920ms - 6.5ms = 43848 samples).
    assert_eq!(seeked.actual_ts.get(), 920 * 48 - 312);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 46);
}

#[test]
fn live_streams_end_cleanly() {
    let mut file = pcm_file();
    file.live = true;

    let bytes = file.to_bytes(&Track::pcm());

    // A complete live stream.
    let mut reader = open(bytes.clone());
    assert_eq!(read_all(&mut reader).len(), 100);
    assert!(reader.next_packet().unwrap().is_none());

    // A live stream that was cut in the middle of a block.
    let mut reader = open(bytes[..bytes.len() - 500].to_vec());
    assert_eq!(read_all(&mut reader).len(), 99);
    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn chapters_without_an_edition_uid() {
    let mut file = pcm_file();

    // ffmpeg does not write an `EditionUID`.
    let atom = |uid: u64, start: u64, name: &str| {
        el(
            &[0xb6],
            &[
                uint(&[0x73, 0xc4], uid),
                uint(&[0x91], start),
                el(&[0x80], &[string(&[0x85], name), string(&[0x43, 0x7c], "eng")].concat()),
            ]
            .concat(),
        )
    };

    let edition = el(&[0x45, 0xb9], &[atom(1, 0, "one"), atom(2, 500_000_000, "two")].concat());
    file.head.push(el(ID_CHAPTERS, &edition));

    let reader = open(file.to_bytes(&Track::pcm()));

    let chapters = reader.chapters().expect("chapters should be present");
    assert_eq!(chapters.items.len(), 2);
}

#[test]
fn cover_attachments_are_visuals() {
    let mut file = pcm_file();

    let attached = |uid: u64, name: &str, media_type: &str, data: &[u8]| {
        el(
            &[0x61, 0xa7],
            &[
                string(&[0x46, 0x6e], name),
                string(&[0x46, 0x60], media_type),
                el(&[0x46, 0x5c], data),
                uint(&[0x46, 0xae], uid),
            ]
            .concat(),
        )
    };

    file.head.push(el(
        ID_ATTACHMENTS,
        &[
            attached(1, "cover.png", "image/png", &[0x89, b'P', b'N', b'G']),
            attached(2, "notes.txt", "text/plain", b"hello"),
            attached(3, "back.jpg", "image/jpeg", &[0xff, 0xd8]),
        ]
        .concat(),
    ));

    let mut reader = open(file.to_bytes(&Track::pcm()));

    // All attachments are still available as attachments.
    assert_eq!(reader.attachments().len(), 3);

    let metadata = reader.metadata();
    let revision = metadata.current().expect("there should be a metadata revision");

    // But only images are visuals.
    assert_eq!(revision.media.visuals.len(), 2);
    assert_eq!(revision.media.visuals[0].usage, Some(StandardVisualKey::FrontCover));
    assert_eq!(revision.media.visuals[0].media_type.as_deref(), Some("image/png"));
    assert_eq!(revision.media.visuals[1].usage, Some(StandardVisualKey::BackCover));
}

#[test]
fn common_tag_names_map_to_standard_tags() {
    let mut file = pcm_file();

    let simple = |name: &str, value: &str| {
        el(&[0x67, 0xc8], &[string(&[0x45, 0xa3], name), string(&[0x44, 0x87], value)].concat())
    };

    // As written by ffmpeg: no targets.
    let tag = el(
        &[0x73, 0x73],
        &[
            el(&[0x63, 0xc0], &[]),
            simple("DATE", "2016-04-01"),
            simple("REPLAYGAIN_TRACK_GAIN", "-6.5 dB"),
            simple("REPLAYGAIN_ALBUM_PEAK", "0.99"),
            simple("MUSICBRAINZ_TRACKID", "abc"),
            simple("musicbrainz_albumid", "def"),
            simple("DATE_RELEASED", "2017"),
        ]
        .concat(),
    );
    file.head.push(el(ID_TAGS, &tag));

    let mut reader = open(file.to_bytes(&Track::pcm()));

    let metadata = reader.metadata();
    let revision = metadata.current().expect("there should be a metadata revision");

    let std_tags: Vec<&StandardTag> =
        revision.media.tags.iter().filter_map(|tag| tag.std.as_ref()).collect();

    let has = |wanted: StandardTag| std_tags.contains(&&wanted);
    let arc = |s: &str| std::sync::Arc::new(s.to_string());

    assert!(has(StandardTag::RecordingDate(arc("2016-04-01"))));
    assert!(has(StandardTag::ReleaseDate(arc("2017"))));
    assert!(has(StandardTag::ReplayGainTrackGain(arc("-6.5 dB"))));
    assert!(has(StandardTag::ReplayGainAlbumPeak(arc("0.99"))));
    assert!(has(StandardTag::MusicBrainzTrackId(arc("abc"))));
    assert!(has(StandardTag::MusicBrainzAlbumId(arc("def"))));
}
