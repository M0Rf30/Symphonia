// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "writer")]
#![allow(clippy::unwrap_used)]

use std::io::Cursor;

use symphonia_core::errors::Error;
use symphonia_core::formats::FormatOptions;
use symphonia_core::formats::FormatReader;
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_ogg::OggReader;
use symphonia_format_ogg::writer::OggPacketWriter;

const FRAMES_PER_PACKET: i64 = 960;

/// Builds one Opus link with `n_packets` 20 ms audio packets, each on its own page.
fn opus_link(serial: u32, n_packets: usize) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.extend_from_slice(&[1, 2]); // version, channels
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // gain
    head.push(0); // mapping family

    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&0u32.to_le_bytes()); // vendor length
    tags.extend_from_slice(&0u32.to_le_bytes()); // comment count

    let mut w = OggPacketWriter::new(Vec::new(), serial);
    w.set_page_target(1);
    w.write_packet(&head, 0, false).unwrap();
    w.flush_page().unwrap();
    w.write_packet(&tags, 0, false).unwrap();
    w.flush_page().unwrap();

    // TOC 0xFC: CELT fullband, 20 ms, one frame (code 0).
    let mut pkt = vec![0xfc];
    pkt.extend_from_slice(&[0x55; 40]);

    for i in 0..n_packets {
        let last = i + 1 == n_packets;
        w.write_packet(&pkt, (i as i64 + 1) * FRAMES_PER_PACKET, last).unwrap();
    }
    w.finish().unwrap()
}

fn open(data: Vec<u8>) -> OggReader<'static> {
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default());
    OggReader::try_new(mss, FormatOptions::default()).unwrap()
}

/// Packets must carry `dts == pts` (audio), and pts must advance by the packet duration.
#[test]
fn packet_dts_equals_pts() {
    let mut reader = open(opus_link(0x1234, 5));

    let mut n = 0;
    while let Some(pkt) = reader.next_packet().unwrap() {
        assert_eq!(pkt.dts, pkt.pts, "packet {n}");
        assert_eq!(pkt.pts.get(), n * FRAMES_PER_PACKET, "packet {n}");
        n += 1;
    }
    assert_eq!(n, 5);
}

/// A chained stream that is cut inside the second link must report an unexpected EOF, not a
/// clean end of stream.
#[test]
fn chained_stream_truncated_in_second_link_is_an_error() {
    let link1 = opus_link(0x1111, 3);
    let link2 = opus_link(0x2222, 3);

    // Cut at several positions inside the second link, from within the head page (the new link
    // never yields a packet) to within its audio packets.
    for cut in [10usize, 60, link2.len() / 2] {
        let mut data = link1.clone();
        data.extend_from_slice(&link2[..cut]);

        let mut reader = open(data);

        let mut n = 0;
        let err = loop {
            match reader.next_packet() {
                Ok(Some(_)) => n += 1,
                Ok(None) => panic!("cut={cut}: truncated chain ended cleanly after {n} packets"),
                // The reset signals the start of the second link. Packets of the second link
                // that were fully received may follow, then the error.
                Err(Error::ResetRequired) => continue,
                Err(e) => break e,
            }
        };
        match err {
            Error::IoError(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            e => panic!("cut={cut}: unexpected error {e}"),
        }
    }
}

/// An intact chained stream still ends cleanly.
#[test]
fn chained_stream_intact_ends_cleanly() {
    let mut data = opus_link(0x1111, 3);
    data.extend_from_slice(&opus_link(0x2222, 3));

    let mut reader = open(data);
    let mut n = 0;
    loop {
        match reader.next_packet() {
            Ok(Some(_)) => n += 1,
            Ok(None) => break,
            Err(Error::ResetRequired) => (),
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(n, 6);
}

/// Builds an Opus link whose first page has a granule position smaller than the duration of the
/// packets that complete on it, i.e. a stream whose start is trimmed (Vorbis I, appendix A.2).
fn opus_link_with_trimmed_start(serial: u32, trim: i64) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);

    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&[0; 8]);

    let mut w = OggPacketWriter::new(Vec::new(), serial);
    w.set_page_target(1);
    w.write_packet(&head, 0, false).unwrap();
    w.flush_page().unwrap();
    w.write_packet(&tags, 0, false).unwrap();
    w.flush_page().unwrap();

    let mut pkt = vec![0xfc];
    pkt.extend_from_slice(&[0x55; 40]);

    for i in 0..3 {
        w.write_packet(&pkt, (i + 1) * FRAMES_PER_PACKET - trim, i == 2).unwrap();
    }
    w.finish().unwrap()
}

/// A negative start granule position discards all the leading frames before timestamp 0, and
/// the delay covers them (Vorbis I, appendix A.2).
#[test]
fn negative_start_granule_trims_leading_frames() {
    let trim = 300;
    let mut reader = open(opus_link_with_trimmed_start(0x77, trim));

    let track = &reader.tracks()[0];
    assert_eq!(track.start_ts.get(), -trim);
    assert_eq!(track.delay, Some(trim as u32));
    assert_eq!(track.num_frames, Some((3 * FRAMES_PER_PACKET - trim) as u64));

    let mut total = 0;
    let mut first = true;
    while let Some(pkt) = reader.next_packet().unwrap() {
        if first {
            assert_eq!(pkt.pts.get(), -trim);
            assert_eq!(pkt.trim_start.get(), trim as u64);
            first = false;
        }
        total += pkt.dur.get();
    }
    assert_eq!(total, (3 * FRAMES_PER_PACKET - trim) as u64);
}

/// The narrowband Speex encoder and decoder lookahead (`SPEEX_GET_LOOKAHEAD`), in samples.
const SPEEX_NB_ENCODER_LOOKAHEAD: i64 = 40;
const SPEEX_NB_DECODER_LOOKAHEAD: i64 = 40;
/// Samples of a narrowband Speex frame.
const SPEEX_NB_FRAME_SIZE: i64 = 160;

/// How the final granule position of a Speex link is written.
#[derive(Copy, Clone)]
enum SpeexEnd {
    /// Like `speexenc`: the number of source samples (clamped).
    Source(i64),
    /// Like libavformat: the same rule as every other page (the packets end, less the encoder
    /// lookahead).
    Unclamped,
}

/// Builds a narrowband Speex link with `n_packets` packets of `frames_per_packet` frames, one
/// packet per page, and `extra_headers` additional header packets. The granule positions
/// exclude the encoder lookahead, like those of `speexenc`.
fn speex_link(
    serial: u32,
    frames_per_packet: u32,
    n_packets: usize,
    extra_headers: u32,
    end: SpeexEnd,
) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"Speex   ");
    head.extend_from_slice(&[0; 20]);
    head.extend_from_slice(&1u32.to_le_bytes()); // version id
    head.extend_from_slice(&80u32.to_le_bytes()); // header size
    head.extend_from_slice(&8000u32.to_le_bytes()); // rate
    head.extend_from_slice(&0u32.to_le_bytes()); // mode: narrowband
    head.extend_from_slice(&4u32.to_le_bytes()); // mode bitstream version
    head.extend_from_slice(&1u32.to_le_bytes()); // channels
    head.extend_from_slice(&(-1i32).to_le_bytes()); // bitrate
    head.extend_from_slice(&(SPEEX_NB_FRAME_SIZE as u32).to_le_bytes()); // frame size
    head.extend_from_slice(&0u32.to_le_bytes()); // vbr
    head.extend_from_slice(&frames_per_packet.to_le_bytes());
    head.extend_from_slice(&extra_headers.to_le_bytes());
    head.extend_from_slice(&[0; 8]);

    // Vendor "v" and two comments.
    let mut tags = Vec::new();
    tags.extend_from_slice(&1u32.to_le_bytes());
    tags.push(b'v');
    tags.extend_from_slice(&2u32.to_le_bytes());
    for c in [&b"TITLE=Song"[..], &b"ARTIST=Band"[..]] {
        tags.extend_from_slice(&(c.len() as u32).to_le_bytes());
        tags.extend_from_slice(c);
    }

    let mut w = OggPacketWriter::new(Vec::new(), serial);
    w.set_page_target(1);
    w.write_packet(&head, 0, false).unwrap();
    w.flush_page().unwrap();
    w.write_packet(&tags, 0, false).unwrap();
    w.flush_page().unwrap();

    // Additional header packets (e.g. extra metadata). Their content is opaque to the mapper.
    for _ in 0..extra_headers {
        w.write_packet(b"extra header packet", 0, false).unwrap();
        w.flush_page().unwrap();
    }

    let pkt = [0x55u8; 30];
    let dur = SPEEX_NB_FRAME_SIZE * i64::from(frames_per_packet);

    for i in 0..n_packets {
        let last = i + 1 == n_packets;
        let mut granule = (i as i64 + 1) * dur - SPEEX_NB_ENCODER_LOOKAHEAD;

        if let (true, SpeexEnd::Source(n)) = (last, end) {
            granule = granule.min(n);
        }
        w.write_packet(&pkt, granule, last).unwrap();
    }
    w.finish().unwrap()
}

/// The sum of the durations of all the packets of a reader (the samples a gapless decoder
/// outputs).
fn total_packet_dur(reader: &mut OggReader<'_>) -> u64 {
    let mut total = 0;
    while let Some(pkt) = reader.next_packet().unwrap() {
        total += pkt.dur.get();
    }
    total
}

/// A Speex stream is mapped: codec parameters, tags, the delay of the encode-decode chain, packet
/// timing from the header and the granule positions, and seeking.
#[test]
fn speex_mapping_timing_and_seek() {
    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX;
    use symphonia_core::formats::{SeekMode, SeekTo};
    use symphonia_core::units::{Duration, Timestamp};

    const DELAY: i64 = SPEEX_NB_ENCODER_LOOKAHEAD + SPEEX_NB_DECODER_LOOKAHEAD;
    const PACKETS: usize = 200;
    // The source ends in the middle of the last packet.
    const SOURCE: i64 = PACKETS as i64 * 320 - SPEEX_NB_ENCODER_LOOKAHEAD - 100;

    let mut reader = open(speex_link(0x55, 2, PACKETS, 0, SpeexEnd::Source(SOURCE)));

    let track = &reader.tracks()[0];
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("speex track must have audio codec parameters");
    };
    assert_eq!(params.codec, CODEC_ID_SPEEX);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.max_frames_per_packet, Some(320));
    assert_eq!(params.extra_data.as_ref().map(|e| e.len()), Some(80));
    // Timestamp 0 is the first source sample: decoded sample 80, and the first packet starts
    // at the negative timestamp -80.
    assert_eq!(track.start_ts.get(), -DELAY);
    assert_eq!(track.delay, Some(DELAY as u32));
    assert_eq!(track.num_frames, Some(SOURCE as u64));

    // The Vorbis comments of the second header packet are the metadata of the stream.
    let tags: Vec<_> =
        reader.metadata().current().unwrap().media.tags.iter().map(|t| t.raw.clone()).collect();
    assert!(tags.iter().any(|t| t.key == "TITLE"));
    assert!(tags.iter().any(|t| t.key == "ARTIST"));

    // Read from the start: the first packet carries the delay, the last the padding.
    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!(first.pts.get(), -DELAY);
    assert_eq!(first.trim_start, Duration::new(DELAY as u64));
    assert_eq!(first.dur, Duration::new(320 - DELAY as u64));
    let second = reader.next_packet().unwrap().unwrap();
    assert_eq!(second.pts.get(), 320 - DELAY);
    assert_eq!(second.trim_start, Duration::ZERO);
    assert_eq!(second.dur, Duration::new(320));

    let mut total = first.dur.get() + second.dur.get();
    let mut last = second;
    while let Some(pkt) = reader.next_packet().unwrap() {
        total += pkt.dur.get();
        last = pkt;
    }
    assert_eq!(total, SOURCE as u64);
    // The packets end at the decoded sample `PACKETS * 320`, the stream at the source end.
    assert_eq!(last.trim_end, Duration::new((PACKETS as i64 * 320 - DELAY - SOURCE) as u64));

    // Seek into the middle of the stream. One second (the sample rate) is decoded and discarded
    // before the target, and the seek lands on a packet boundary at or before that.
    let mut reader = open(speex_link(0x55, 2, PACKETS, 0, SpeexEnd::Source(SOURCE)));
    let target = 40_000;
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0x55 })
        .unwrap();
    assert!(seeked.actual_ts <= Timestamp::new(target - 8000));
    assert!(seeked.actual_ts >= Timestamp::new(target - 8000 - 2 * 320));

    // The next packet starts exactly there.
    let next = reader.next_packet().unwrap().unwrap();
    assert_eq!(next.pts, seeked.actual_ts);

    // A seek to the start lands on the first packet again, which carries the delay.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0x55 })
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), -DELAY);
    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!(first.pts.get(), -DELAY);
    assert_eq!(first.trim_start, Duration::new(DELAY as u64));
}

/// The decoded samples that are output are the same, however many frames a packet carries, and
/// the last page of a stream not written by `speexenc` (libavformat does not clamp the granule
/// position to the source length) does not trim anything.
#[test]
fn speex_total_duration_by_muxer() {
    const DELAY: u64 = (SPEEX_NB_ENCODER_LOOKAHEAD + SPEEX_NB_DECODER_LOOKAHEAD) as u64;

    for frames_per_packet in [1u32, 2, 5] {
        let dur = (SPEEX_NB_FRAME_SIZE as u64) * u64::from(frames_per_packet);
        let n_packets = 50 / frames_per_packet as usize * 2;
        let decoded = n_packets as u64 * dur;

        // `speexenc`: the source ends 100 samples before the last decoded sample is needed.
        let source =
            decoded - SPEEX_NB_ENCODER_LOOKAHEAD as u64 - SPEEX_NB_DECODER_LOOKAHEAD as u64 - 100;
        let mut reader =
            open(speex_link(1, frames_per_packet, n_packets, 0, SpeexEnd::Source(source as i64)));
        assert_eq!(total_packet_dur(&mut reader), source, "speexenc, {frames_per_packet} frames");

        // libavformat: everything after the chain delay is output.
        let mut reader = open(speex_link(1, frames_per_packet, n_packets, 0, SpeexEnd::Unclamped));
        assert_eq!(
            total_packet_dur(&mut reader),
            decoded - DELAY,
            "libavformat, {frames_per_packet} frames"
        );
    }
}

/// Additional header packets are skipped (they are not audio).
#[test]
fn speex_extra_header_packets_are_skipped() {
    let mut reader = open(speex_link(1, 1, 10, 3, SpeexEnd::Unclamped));

    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!(first.pts.get(), -80);
    assert_eq!(first.data.len(), 30);

    let mut n = 1;
    while reader.next_packet().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 10);
}

/// Each link of a chained Speex stream starts over with its own delay.
#[test]
fn speex_chained_links_restart_the_timeline() {
    let mut data = speex_link(0x11, 2, 20, 0, SpeexEnd::Unclamped);
    data.extend_from_slice(&speex_link(0x22, 1, 20, 0, SpeexEnd::Unclamped));

    let mut reader = open(data);
    let mut links: Vec<(u32, i64, u64)> = Vec::new();

    loop {
        match reader.next_packet() {
            Ok(Some(pkt)) => match links.last_mut() {
                Some((serial, _, total)) if *serial == pkt.track_id => *total += pkt.dur.get(),
                _ => links.push((pkt.track_id, pkt.pts.get(), pkt.dur.get())),
            },
            Ok(None) => break,
            Err(Error::ResetRequired) => (),
            Err(e) => panic!("{e}"),
        }
    }

    // 20 packets of 320 and of 160 decoded samples, less the 80 sample chain delay.
    assert_eq!(links, vec![(0x11, -80, 20 * 320 - 80), (0x22, -80, 20 * 160 - 80)]);
}

/// CRC-8 (polynomial 0x07) of a FLAC frame header.
fn flac_crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
        crc
    })
}

/// Builds an Ogg FLAC stream of `n_frames` frames of 32768 samples. Each frame is
/// `frame_len` bytes: larger than a page, so the frames span pages, the intermediate pages have
/// no granule position, and the first frame begins before the first page it completes on.
fn flac_link_with_large_frames(serial: u32, n_frames: usize, frame_len: usize) -> Vec<u8> {
    const BLOCK: i64 = 32768;

    let mut head = vec![0x7f];
    head.extend_from_slice(b"FLAC");
    head.extend_from_slice(&[1, 0]);
    head.extend_from_slice(&1u16.to_be_bytes());
    head.extend_from_slice(b"fLaC");
    head.extend_from_slice(&[0x00, 0, 0, 34]);

    let mut info = Vec::new();
    info.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    info.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    info.extend_from_slice(&[0; 6]);
    let packed = (44100u64 << 44) | (1 << 41) | (15 << 36) | (n_frames as u64 * BLOCK as u64);
    info.extend_from_slice(&packed.to_be_bytes());
    info.extend_from_slice(&[0; 16]);
    head.extend_from_slice(&info);

    // Last metadata block: Vorbis comment, no vendor, no comments.
    let tags = [0x84, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0];

    let mut w = OggPacketWriter::new(Vec::new(), serial);
    w.write_packet(&head, 0, false).unwrap();
    w.flush_page().unwrap();

    // The comment block shares its page with the beginning of the first frame.
    w.write_packet(&tags, 0, false).unwrap();

    for i in 0..n_frames {
        // Fixed block size frame: 32768 samples (0xf), rate and bit depth from the stream info,
        // stereo (0x1), frame number, CRC-8.
        let mut frame = vec![0xff, 0xf8, 0xf0, 0x10, i as u8];
        frame.push(flac_crc8(&frame));
        frame.resize(frame_len, 0x55);

        let granule = (i as i64 + 1) * BLOCK;
        w.write_packet(&frame, granule, i + 1 == n_frames).unwrap();
    }
    w.finish().unwrap()
}

/// Packets larger than a page: the timeline starts at 0 and seeking works, including to the very
/// start where the first packet begins on a page before the first page with a granule position.
#[test]
fn large_packets_spanning_pages_seek() {
    use symphonia_core::formats::{SeekMode, SeekTo};
    use symphonia_core::units::Timestamp;

    const BLOCK: i64 = 32768;
    let data = flac_link_with_large_frames(0x66, 30, 150_000);

    let reader = open(data.clone());
    let track = &reader.tracks()[0];
    assert_eq!(track.start_ts.get(), 0);
    assert_eq!(track.num_frames, Some(30 * BLOCK as u64));
    assert_eq!(track.delay, None);

    for target in [0, 1, 10 * BLOCK + 5, 29 * BLOCK + 100, 17 * BLOCK + 1] {
        let mut reader = open(data.clone());
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0x66 },
            )
            .unwrap();
        let expected = target / BLOCK * BLOCK;
        assert_eq!(seeked.actual_ts.get(), expected, "target {target}");

        let pkt = reader.next_packet().unwrap().unwrap();
        assert_eq!(pkt.pts.get(), expected, "target {target}");
        assert_eq!(pkt.dur.get(), BLOCK as u64);
    }
}
