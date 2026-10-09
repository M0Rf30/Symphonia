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
    } else if n < 0x3fff {
        vec![0x40 | (n >> 8) as u8, n as u8]
    } else if n < 0x1f_ffff {
        vec![0x20 | (n >> 16) as u8, (n >> 8) as u8, n as u8]
    } else {
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
    /// `CodecPrivate`.
    codec_private: Option<Vec<u8>>,
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
            codec_private: None,
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
            codec_private: None,
        }
    }

    /// 16-bit stereo FLAC at 44.1kHz with a fixed block size of 4096.
    fn flac() -> Self {
        // The STREAMINFO block.
        let mut info = Vec::new();
        info.extend(4096u16.to_be_bytes());
        info.extend(4096u16.to_be_bytes());
        info.extend([0u8; 6]);
        let packed: u64 = (44100u64 << 44) | (1 << 41) | (15 << 36) | 40960;
        info.extend(packed.to_be_bytes());
        info.extend([0u8; 16]);

        let mut private = b"fLaC".to_vec();
        private.extend([0x80, 0, 0, info.len() as u8]);
        private.extend(info);

        Track {
            codec: "A_FLAC",
            sample_rate: 44100.0,
            channels: 2,
            bit_depth: Some(16),
            codec_delay: 0,
            seek_pre_roll: 0,
            default_duration: None,
            codec_private: Some(private),
        }
    }

    /// The track entry.
    fn entry(&self) -> Vec<u8> {
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
        if let Some(private) = &self.codec_private {
            entry.push(el(&[0x63, 0xa2], private));
        }

        el(&[0xae], &entry.concat())
    }
}

/// The entry of a subtitle track, track 2.
fn subtitle_entry() -> Vec<u8> {
    el(
        &[0xae],
        &[
            uint(&[0xd7], 2),
            uint(&[0x73, 0xc5], 2),
            uint(&[0x83], 0x11),
            string(&[0x86], "S_TEXT/UTF8"),
        ]
        .concat(),
    )
}

/// A block (of track 1, unless specified otherwise).
struct Block {
    /// The track number.
    track: u8,
    /// The absolute timestamp of the block in milliseconds.
    ts: i64,
    /// The length of the block's data. The first byte of the data is the index of the block
    /// unless `fill` is set.
    len: usize,
    /// The duration of the block in milliseconds. Forces a block group.
    duration: Option<u64>,
    /// The discard padding of the block in nanoseconds. Forces a block group.
    padding: Option<i64>,
    /// The byte the data of the block is filled with, instead of the index of the block.
    fill: Option<u8>,
    /// The data of the block. If not set, `len` bytes of the index of the block.
    data: Option<Vec<u8>>,
}

impl Block {
    fn new(ts: i64, len: usize) -> Self {
        Block { track: 1, ts, len, duration: None, padding: None, fill: None, data: None }
    }

    fn filled(ts: i64, len: usize, fill: u8) -> Self {
        Block { fill: Some(fill), ..Block::new(ts, len) }
    }

    /// A block with the given data.
    fn with_data(ts: i64, data: Vec<u8>) -> Self {
        let len = data.len();
        Block { data: Some(data), ..Block::new(ts, len) }
    }
}

/// A cue point.
struct Cue {
    /// The time of the cue in milliseconds.
    time: u64,
    cluster: usize,
    /// The index of the block in the cluster the cue refers to.
    block: usize,
    /// The track number the cue is for.
    track: u64,
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
    /// If the file has a subtitle track, track 2, in addition to the audio track.
    subtitles: bool,
}

impl File {
    fn to_bytes(&self, track: &Track) -> Vec<u8> {
        let mut entries = track.entry();
        if self.subtitles {
            entries.extend(subtitle_entry());
        }

        let mut head = [info(), el(ID_TRACKS, &entries)].concat();
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
                let mut payload = vec![0x80 | blk.track, (rel >> 8) as u8, rel as u8];

                if blk.duration.is_some() || blk.padding.is_some() {
                    payload.push(0x00);
                    payload.extend(
                        blk.data
                            .clone()
                            .unwrap_or_else(|| vec![blk.fill.unwrap_or(index); blk.len]),
                    );

                    let mut group = vec![el(&[0xa1], &payload)];
                    if let Some(dur) = blk.duration {
                        group.push(uint(&[0x9b], dur));
                    }
                    if let Some(padding) = blk.padding {
                        group.push(sint(&[0x75, 0xa2], padding));
                    }
                    data.extend(el(&[0xa0], &group.concat()));
                } else {
                    payload.push(0x80);
                    payload.extend(
                        blk.data
                            .clone()
                            .unwrap_or_else(|| vec![blk.fill.unwrap_or(index); blk.len]),
                    );
                    data.extend(el(&[0xa3], &payload));
                }

                index = index.wrapping_add(1);
            }

            let mut cluster = ID_CLUSTER.to_vec();
            if self.live {
                cluster.extend(UNKNOWN_SIZE);
                cluster.extend(&data);
            } else {
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
                        uint(&[0xf7], cue.track),
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
        } else {
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
        Cue { time: 120, cluster: 0, block: 12, track: 1 },
        Cue { time: 250, cluster: 1, block: 0, track: 1 },
        Cue { time: 370, cluster: 1, block: 12, track: 1 },
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

#[test]
fn seek_with_cues_of_another_track_starts_at_the_first_block() {
    let mut file = pcm_file();

    // The only cue point is for another track (e.g., a video track in the same file), and refers
    // to the second block of the cluster. The (audio) block before it must not be skipped.
    file.cues = vec![Cue { time: 0, cluster: 0, block: 1, track: 2 }];

    let mut reader = open(file.to_bytes(&Track::pcm()));

    let seeked = seek_time(&mut reader, 0).unwrap();
    assert_eq!(seeked.required_ts.get(), 0);
    assert_eq!(seeked.actual_ts.get(), 0);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 0);

    // A seek past the cue also starts from the cluster, and is not late.
    let seeked = seek_time(&mut reader, 135).unwrap();
    assert_eq!(seeked.actual_ts.get(), 130 * 48);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 13);
}

#[test]
fn seek_with_a_wrong_cue_position_is_not_late() {
    let mut file = pcm_file();

    // The cue point for the track says the block at time 0 is the 6th block of the cluster.
    file.cues = vec![Cue { time: 0, cluster: 0, block: 5, track: 1 }];

    let mut reader = open(file.to_bytes(&Track::pcm()));

    let seeked = seek_time(&mut reader, 0).unwrap();
    assert_eq!(seeked.actual_ts.get(), 0);
    assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 0);
}

#[test]
fn lossless_block_timestamps_are_sample_exact() {
    // 4096 frame blocks at 44.1kHz do not start at whole milliseconds. The muxer rounds the
    // timestamps (and block durations) to the millisecond, and writes no default duration.
    let mut file = File::default();

    let blocks = (0..10i64)
        .map(|i| {
            let ts = (i * 4096 * 1000 + 22050) / 44100;
            Block {
                track: 1,
                ts,
                len: 100,
                duration: Some(93),
                padding: None,
                fill: None,
                data: None,
            }
        })
        .collect();
    file.clusters.push((0, blocks));

    let mut reader = open(file.to_bytes(&Track::flac()));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 10);

    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.pts.get(), 4096 * i as i64);
        assert_eq!(packet.dur.get(), 4096);
    }

    // Seeks are exact: 0.5s is frame 22050, which is in the block starting at 5 * 4096.
    let seeked = seek_time(&mut reader, 500).unwrap();
    assert_eq!(seeked.required_ts.get(), 22050);
    assert_eq!(seeked.actual_ts.get(), 5 * 4096);

    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!((packet.pts.get(), packet.data[0]), (5 * 4096, 5));

    // So the number of frames to discard after decoding from `actual_ts` to reach the required
    // timestamp is exact too.
    assert_eq!(seeked.required_ts.get() - seeked.actual_ts.get(), 22050 - 20480);
}

/// Six 20ms Opus packets (the first with a 6.5ms pre-skip, and the last with 13.5ms of padding), as
/// written by ffmpeg.
fn opus_file() -> File {
    let mut file = File::default();

    let mut blocks: Vec<Block> =
        [0, 21, 41, 61, 81].iter().map(|&ts| Block::with_data(ts, opus_20ms_packet(0))).collect();

    blocks.push(Block {
        track: 1,
        ts: 101,
        len: 100,
        duration: Some(7),
        padding: Some(13_500_000),
        fill: None,
        data: Some(opus_20ms_packet(5)),
    });

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

/// The table-of-contents bytes of a CELT-only (configuration 31) and a SILK-only (configuration 9)
/// 20ms Opus packet.
const TOC_CELT: u8 = 0xf8;
const TOC_SILK: u8 = 0x48;

fn opus_with_toc(toc: u8) -> MkvReader<'static> {
    let mut file = File::default();

    // 600 packets of 20ms (12s) with exact timestamps.
    file.clusters.push((0, (0..600).map(|i| Block::filled(i * 20, 100, toc)).collect()));

    open(file.to_bytes(&Track::opus()))
}

#[test]
fn opus_celt_seek_backs_off_by_1_5_seconds() {
    let mut reader = opus_with_toc(TOC_CELT);

    // 11s is 528000 samples. The pre-skip is 312 samples, and the pre-roll of CELT is 1.5s (72000
    // samples).
    let seeked = seek_time(&mut reader, 11_000).unwrap();
    assert_eq!(seeked.required_ts.get(), 528_000);
    // The last packet that starts before 528000 - 72000 = 456000 (the packet that starts at
    // 9500ms - 6.5ms = 455688 samples).
    assert_eq!(seeked.actual_ts.get(), 9500 * 48 - 312);

    // Near the start, the seek is clamped to the first packet.
    let seeked = seek_time(&mut reader, 500).unwrap();
    assert_eq!(seeked.actual_ts.get(), -312);
}

#[test]
fn opus_silk_seek_backs_off_by_10_seconds() {
    let mut reader = opus_with_toc(TOC_SILK);

    // Nothing is known about the contents of the stream when the first seek begins, so it first
    // lands with the pre-roll of CELT, finds a SILK packet there, and seeks again with the
    // pre-roll of SILK (10s, 480000 samples): the last packet that starts before 528000 - 480000 =
    // 48000 (the packet that starts at 1000ms - 6.5ms = 47688 samples).
    let seeked = seek_time(&mut reader, 11_000).unwrap();
    assert_eq!(seeked.required_ts.get(), 528_000);
    assert_eq!(seeked.actual_ts.get(), 1000 * 48 - 312);
    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 1000 * 48 - 312);

    // Subsequent seeks use the pre-roll of SILK directly.
    let seeked = seek_time(&mut reader, 6_000).unwrap();
    assert_eq!(seeked.actual_ts.get(), 0 - 312);
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

/// A 100 byte Opus packet of one 20ms CELT frame, tagged with `tag` in its second byte.
fn opus_20ms_packet(tag: u8) -> Vec<u8> {
    opus_packet(0x98, tag)
}

/// A 100 byte Opus packet with the TOC byte `toc`, tagged with `tag` in its second byte.
fn opus_packet(toc: u8, tag: u8) -> Vec<u8> {
    let mut data = vec![toc, tag];
    data.resize(100, 0);
    data
}

// Exact timelines.

/// A pseudo-random number generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// A little-endian bit writer, as used by the Vorbis setup header.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    fn put(&mut self, mut value: u64, bits: usize) {
        for _ in 0..bits {
            if self.len % 8 == 0 {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= ((value & 1) as u8) << (self.len % 8);
            value >>= 1;
            self.len += 1;
        }
    }
}

/// The Xiph laced `CodecPrivate` of a stereo, 44.1kHz Vorbis track with block sizes of 256 and
/// 2048 and two modes: the first selects the short block, the second the long block. The setup
/// header is the smallest one possible (a single codebook, floor, residue, and mapping).
fn vorbis_codec_private() -> Vec<u8> {
    let mut ident = vec![1];
    ident.extend(b"vorbis");
    ident.extend(0u32.to_le_bytes());
    ident.push(2);
    ident.extend(44100u32.to_le_bytes());
    ident.extend([0u8; 12]);
    ident.push(8 | (11 << 4));
    ident.push(1);
    assert_eq!(ident.len(), 30);

    let mut comment = vec![3];
    comment.extend(b"vorbis");
    comment.extend(0u32.to_le_bytes());
    comment.extend(0u32.to_le_bytes());
    comment.push(1);

    let mut w = Bits::default();
    // One codebook with one entry of one dimension.
    w.put(0, 8);
    w.put(0x564342, 24);
    w.put(1, 16);
    w.put(1, 24);
    w.put(0, 2);
    w.put(0, 5);
    w.put(0, 4);
    // One time domain transform.
    w.put(0, 6);
    w.put(0, 16);
    // One floor of type 1, without partitions.
    w.put(0, 6);
    w.put(1, 16);
    w.put(0, 5);
    w.put(0, 2);
    w.put(0, 4);
    // One residue of type 0 with one class and no books.
    w.put(0, 6);
    w.put(0, 16);
    w.put(0, 72);
    w.put(0, 6);
    w.put(0, 8);
    w.put(0, 4);
    // One mapping with one submap and no coupling.
    w.put(0, 6);
    w.put(0, 16);
    w.put(0, 2);
    w.put(0, 2);
    w.put(0, 24);
    // Two modes.
    w.put(1, 6);
    for block_flag in [0, 1] {
        w.put(block_flag, 1);
        w.put(0, 40);
    }
    // Framing.
    w.put(1, 1);

    let mut setup = vec![5];
    setup.extend(b"vorbis");
    setup.extend(w.bytes);

    let mut private = vec![2, ident.len() as u8, comment.len() as u8];
    private.extend(ident);
    private.extend(comment);
    private.extend(setup);
    private
}

fn vorbis_track() -> Track {
    Track {
        codec: "A_VORBIS",
        sample_rate: 44100.0,
        channels: 2,
        bit_depth: None,
        codec_delay: 0,
        seek_pre_roll: 0,
        default_duration: None,
        codec_private: Some(vorbis_codec_private()),
    }
}

/// The packets of a stream, and their exact timeline.
struct Model {
    /// The timestamp, in frames, at which each packet starts, as it is presented by the demuxer.
    pts: Vec<i64>,
    /// The duration, in frames, of each packet, as presented by the demuxer (without the frames
    /// that are to be discarded).
    dur: Vec<u64>,
    /// The number of frames at the start of each packet that are to be discarded.
    trim_start: Vec<u64>,
}

/// Create a Vorbis stream of `n` packets with random block sizes, with timestamps rounded to the
/// millisecond as a muxer does. Every `cue_every` blocks are indexed by a cue point (if
/// non-zero).
fn vorbis_stream(n: usize, cue_every: usize) -> (File, Model) {
    let mut rng = Lcg(7);

    // The block size of each packet: 256 or 2048 (1/4 of the packets are long, but runs of long
    // and short packets are common).
    let mut is_long = false;
    let long: Vec<bool> = (0..n)
        .map(|_| {
            if rng.next() % 3 == 0 {
                is_long = !is_long;
            }
            is_long
        })
        .collect();

    let size = |i: usize| if long[i] { 2048u64 } else { 256 };

    let mut model = Model { pts: Vec::new(), dur: Vec::new(), trim_start: Vec::new() };

    // The first packet decodes to nothing, its frames are all discarded. The audio starts with the
    // second packet, at 0.
    model.pts.push(-(size(0) as i64) / 2);
    model.dur.push(0);
    model.trim_start.push(size(0) / 2);

    for i in 1..n {
        let prev_end = model.pts[i - 1] + (model.dur[i - 1] + model.trim_start[i - 1]) as i64;
        model.pts.push(prev_end);
        model.dur.push(size(i - 1) / 4 + size(i) / 4);
        model.trim_start.push(0);
    }

    // Timestamps in milliseconds are rounded.
    let millis = |pts: i64| (pts * 1000 + 22050).div_euclid(44100);

    let mut file = File::default();
    let mut cues = Vec::new();

    for (c, chunk) in (0..n).collect::<Vec<_>>().chunks(100).enumerate() {
        let blocks: Vec<Block> = chunk
            .iter()
            .map(|&i| {
                // The packet type bit is 0, followed by the mode number, and anything.
                let data = vec![u8::from(long[i]) << 1, (i % 251) as u8, 0xaa, 0x55];
                Block::with_data(millis(model.pts[i]), data)
            })
            .collect();

        for (b, block) in blocks.iter().enumerate() {
            if cue_every != 0 && (c * 100 + b) % cue_every == 0 {
                cues.push(Cue { time: block.ts.max(0) as u64, cluster: c, block: b, track: 1 });
            }
        }

        file.clusters.push((blocks[0].ts.max(0), blocks));
    }

    file.cues = cues;
    (file, model)
}

fn seek_ts(reader: &mut MkvReader<'_>, ts: i64) -> Result<SeekedTo, symphonia_core::errors::Error> {
    reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 1 })
}

/// Check the timeline presented by the demuxer is the model's.
fn assert_timeline(packets: &[Packet], model: &Model, first: usize) {
    for (i, packet) in packets.iter().enumerate() {
        let k = first + i;
        assert_eq!(packet.pts.get(), model.pts[k], "pts of packet {k}");
        assert_eq!(packet.dur.get(), model.dur[k], "dur of packet {k}");
        assert_eq!(packet.trim_start.get(), model.trim_start[k], "trim_start of packet {k}");
        assert_eq!(packet.trim_end.get(), 0);
    }
}

/// The index of the last packet that starts at, or before, `ts`. Or the first packet.
fn packet_at(model: &Model, ts: i64) -> usize {
    model.pts.partition_point(|&pts| pts <= ts).saturating_sub(1)
}

#[test]
fn vorbis_timeline_of_an_unseekable_stream_is_sample_exact() {
    let (file, model) = vorbis_stream(1500, 10);
    let bytes = file.to_bytes(&vorbis_track());

    let source = symphonia_core::io::ReadOnlySource::new(Cursor::new(bytes));
    let mss = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let mut reader = MkvReader::try_new(mss, FormatOptions::default()).expect("file should open");

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 1500);
    assert_timeline(&packets, &model, 0);
}

#[test]
fn vorbis_seeks_in_a_file_with_other_tracks_are_sample_exact() {
    let (mut file, model) = vorbis_stream(1500, 10);
    file.subtitles = true;

    // A subtitle block after every 5th Vorbis block.
    for (_, blocks) in &mut file.clusters {
        let mut with_subtitles = Vec::new();

        for (i, block) in std::mem::take(blocks).into_iter().enumerate() {
            let ts = block.ts;
            with_subtitles.push(block);

            if i % 5 == 4 {
                let mut subtitle = Block::with_data(ts, vec![0x41; 200]);
                subtitle.track = 2;
                with_subtitles.push(subtitle);
            }
        }

        *blocks = with_subtitles;
    }

    // The cue points no longer refer to the right blocks.
    file.cues.clear();

    let mut reader = open(file.to_bytes(&vorbis_track()));
    let mut rng = Lcg(17);
    let end = model.pts[1499] + model.dur[1499] as i64;

    for _ in 0..100 {
        let target = (rng.next() % end as u64) as i64;
        let seeked = seek_ts(&mut reader, target).expect("seek should succeed");

        let expected = packet_at(&model, target - 8820);
        assert_eq!(seeked.actual_ts.get(), model.pts[expected], "target {target}");

        // The first packet of the audio track is next, and the timeline continues from it.
        let packets: Vec<Packet> = (0..30)
            .map_while(|_| reader.next_packet().unwrap())
            .filter(|packet| packet.track_id == 1)
            .collect();

        assert_timeline(&packets, &model, expected);
    }
}

#[test]
fn vorbis_timeline_is_sample_exact() {
    let (file, model) = vorbis_stream(1500, 0);
    let mut reader = open(file.to_bytes(&vorbis_track()));

    // Many packets do not start at a whole millisecond.
    assert!(model.pts.iter().any(|pts| pts * 1000 % 44100 > 1000));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 1500);
    assert_timeline(&packets, &model, 0);

    // The audio starts at 0.
    assert_eq!(packets[1].pts.get(), 0);
}

#[test]
fn vorbis_timeline_starts_at_0_when_the_first_packet_is_timestamped_0() {
    let (mut file, model) = vorbis_stream(300, 0);

    // As mkvmerge: the first packet is timestamped 0.
    file.clusters[0].1[0].ts = 0;

    let mut reader = open(file.to_bytes(&vorbis_track()));
    let packets = read_all(&mut reader);

    assert_timeline(&packets, &model, 0);
}

#[test]
fn vorbis_timeline_is_anchored_again_after_a_gap() {
    let (mut file, model) = vorbis_stream(300, 0);

    // Remove the blocks 100 to 149.
    file.clusters[1].1.drain(..50);

    let mut reader = open(file.to_bytes(&vorbis_track()));
    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 250);

    assert_timeline(&packets[..100], &model, 0);

    // The first packet after the gap is timestamped to the millisecond.
    let offset = packets[100].pts.get() - model.pts[150];
    assert!(offset.abs() <= 22, "the first packet after the gap is {offset} frames off");

    // The packets are as long as the decoder, which overlaps the packet before the gap with the
    // one after it, makes them, and follow each other.
    for (i, pair) in packets.windows(2).enumerate().skip(100) {
        assert_eq!(pair[1].pts, pair[0].pts.saturating_add(pair[0].block_dur()));
        assert_eq!(pair[1].dur.get(), model.dur[i + 51], "dur of packet {}", i + 1);
    }
}

#[test]
fn vorbis_timeline_of_a_stream_not_starting_at_0_is_anchored_to_the_first_block() {
    let (mut file, model) = vorbis_stream(300, 0);

    // Shift all blocks by 10s.
    for (cluster_ts, blocks) in &mut file.clusters {
        *cluster_ts += 10_000;
        for block in blocks {
            block.ts += 10_000;
        }
    }

    let mut reader = open(file.to_bytes(&vorbis_track()));
    let packets = read_all(&mut reader);

    // The first packet is timestamped to the millisecond. The rest is exact.
    assert!((packets[0].pts.get() - (441_000 + model.pts[0])).abs() <= 45);
    for pair in packets.windows(2) {
        assert_eq!(pair[1].pts, pair[0].pts.saturating_add(pair[0].block_dur()));
    }
}

fn check_vorbis_seeks(cue_every: usize) {
    let (file, model) = vorbis_stream(1500, cue_every);
    let mut reader = open(file.to_bytes(&vorbis_track()));

    let end = model.pts[1499] + model.dur[1499] as i64;
    let mut rng = Lcg(99);

    let mut targets = vec![0, 1, 127, 128, 129, 8819, 8820, 8821, end - 1, end - 5000, 44100];
    targets.extend((0..300).map(|_| (rng.next() % end as u64) as i64));

    for target in targets {
        let seeked = seek_ts(&mut reader, target).expect("seek should succeed");
        assert_eq!(seeked.required_ts.get(), target);

        // The default seek pre-roll of Vorbis is 200ms: 8820 frames. The landing packet is the
        // last one at or before that, to the frame.
        let expected = packet_at(&model, target - 8820);
        assert_eq!(seeked.actual_ts.get(), model.pts[expected], "target {target}");

        // The packets read afterwards have an exact timeline, whose durations depend on the
        // blocks before them.
        let packets: Vec<Packet> = (0..20).map_while(|_| reader.next_packet().unwrap()).collect();
        assert_timeline(&packets, &model, expected);

        assert!(model.pts[expected] <= target.max(0));
    }

    // A seek by time is also exact: 7.5s is 330750 frames.
    let seeked = seek_time(&mut reader, 7500).unwrap();
    assert_eq!(seeked.required_ts.get(), 330750);
    assert_eq!(seeked.actual_ts.get(), model.pts[packet_at(&model, 330750 - 8820)]);

    // Reading to the end after a seek continues the timeline.
    let packets = read_all(&mut reader);
    let first = packet_at(&model, 330750 - 8820);
    assert_timeline(&packets[..], &model, first);
}

#[test]
fn vorbis_seeks_with_cues_are_sample_exact() {
    check_vorbis_seeks(10);
}

#[test]
fn vorbis_seeks_without_cues_are_sample_exact() {
    check_vorbis_seeks(0);
}

#[test]
fn vorbis_seeks_with_sparse_cues_are_sample_exact() {
    check_vorbis_seeks(250);
}

/// The TOC byte, and the duration in frames, of Opus packets.
const OPUS_PACKETS: [(u8, u64); 8] = [
    (0x80, 120),
    (0x88, 240),
    (0x90, 480),
    (0x98, 960),
    (0x10, 1920),
    (0x18, 2880),
    (0x99, 1920),
    (0x68, 960),
];

/// An Opus stream without a default duration, of packets of varying durations.
fn opus_stream(n: usize, cue_every: usize) -> (File, Model) {
    let mut rng = Lcg(3);
    let mut kinds: Vec<usize> = (0..n).map(|_| (rng.next() % 8) as usize).collect();

    // The first packet must be longer than the pre-skip.
    kinds[0] = 3;

    let mut model = Model { pts: Vec::new(), dur: Vec::new(), trim_start: Vec::new() };
    let mut pts = -312i64;

    for &kind in &kinds {
        let dur = OPUS_PACKETS[kind].1;
        model.pts.push(pts);
        model.dur.push(dur);
        pts += dur as i64;
    }

    // The 312 frames of pre-skip are trimmed from the first packet.
    model.dur[0] -= 312;
    model.trim_start.push(312);
    model.trim_start.extend(std::iter::repeat_n(0, n - 1));

    // The timestamp of a block is the start of its frames in milliseconds, rounded, plus the
    // codec delay.
    let millis = |pts: i64| ((pts + 312) * 1000 + 24000).div_euclid(48000);

    let mut file = File::default();
    let mut cues = Vec::new();

    for (c, chunk) in (0..n).collect::<Vec<_>>().chunks(100).enumerate() {
        let blocks: Vec<Block> = chunk
            .iter()
            .map(|&i| {
                Block::with_data(
                    millis(model.pts[i]),
                    opus_packet(OPUS_PACKETS[kinds[i]].0, i as u8),
                )
            })
            .collect();

        for (b, block) in blocks.iter().enumerate() {
            if cue_every != 0 && (c * 100 + b) % cue_every == 0 {
                cues.push(Cue { time: block.ts as u64, cluster: c, block: b, track: 1 });
            }
        }

        file.clusters.push((blocks[0].ts, blocks));
    }

    file.cues = cues;
    (file, model)
}

fn opus_track_without_default_duration() -> Track {
    Track { default_duration: None, ..Track::opus() }
}

// The packets of the model have no padding, and the model's duration of the first packet is
// that of its packet less the delay.
#[test]
fn opus_timeline_of_varying_packets_is_sample_exact() {
    let (file, model) = opus_stream(1000, 0);
    let mut reader = open(file.to_bytes(&opus_track_without_default_duration()));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 1000);

    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.pts.get(), model.pts[i], "pts of packet {i}");
        assert_eq!(packet.dur.get(), model.dur[i], "dur of packet {i}");
        assert_eq!(packet.trim_start.get(), model.trim_start[i], "trim_start of packet {i}");
    }
}

#[test]
fn opus_seeks_in_varying_packets_are_sample_exact() {
    for cue_every in [0, 7, 100] {
        let (file, model) = opus_stream(1000, cue_every);
        let mut reader = open(file.to_bytes(&opus_track_without_default_duration()));

        let end = model.pts[999] + model.dur[999] as i64;
        let mut rng = Lcg(5);

        let mut targets = vec![0, 1, 311, 312, 3839, 3840, 3841, 71_999, 72_000, 72_001, end];
        targets.extend((0..200).map(|_| (rng.next() % end as u64) as i64));

        let mut silk = false;

        for target in targets {
            let seeked = seek_ts(&mut reader, target).expect("seek should succeed");
            assert_eq!(seeked.required_ts.get(), target);

            // The seek pre-roll is 1.5s (72000 frames) until a SILK or Hybrid packet is seen
            // while seeking, then 10s (480000 frames), see `TrackState::effective_seek_pre_roll`.
            let celt = packet_at(&model, target - 72_000);
            let silk_expected = packet_at(&model, target - 480_000);

            let expected = if silk {
                silk_expected
            }
            else {
                let found = (0..model.pts.len())
                    .find(|&k| model.pts[k] == seeked.actual_ts.get())
                    .expect("seek should land on a packet");
                assert!(found == celt || found == silk_expected, "target {target}");
                silk = found == silk_expected && silk_expected != celt;
                found
            };

            assert_eq!(seeked.actual_ts.get(), model.pts[expected], "target {target}");

            let packets: Vec<Packet> =
                (0..20).map_while(|_| reader.next_packet().unwrap()).collect();

            for (i, packet) in packets.iter().enumerate() {
                let k = expected + i;
                assert_eq!(packet.pts.get(), model.pts[k], "pts of packet {k}");
                assert_eq!(packet.dur.get(), model.dur[k], "dur of packet {k}");
            }
        }
    }
}

#[test]
fn opus_timeline_is_anchored_again_after_a_gap() {
    let (mut file, model) = opus_stream(300, 0);

    // Remove the blocks 100 to 149 from the second cluster.
    file.clusters[1].1.drain(..50);

    let mut reader = open(file.to_bytes(&opus_track_without_default_duration()));
    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 250);

    // The timeline before the gap is exact.
    assert_eq!(packets[0].pts.get(), model.pts[0]);
    for (i, packet) in packets[..100].iter().enumerate() {
        assert_eq!(packet.pts.get(), model.pts[i]);
    }

    // The gap in the timeline is that of the missing packets, to the precision of the timestamp of
    // the first block after the gap (0.5ms: 24 frames), and exact after that.
    let offset = packets[100].pts.get() - model.pts[150];
    assert!(offset.abs() <= 24, "the first packet after the gap is {offset} frames off");

    for (i, packet) in packets.iter().enumerate().skip(100) {
        assert_eq!(packet.pts.get() - model.pts[i + 50], offset, "pts of packet {i}");
        assert_eq!(packet.dur.get(), model.dur[i + 50]);
    }
}
