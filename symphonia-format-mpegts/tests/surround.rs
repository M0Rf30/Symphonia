#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the AC-3, E-AC-3, DTS, and TrueHD streams, with synthetic transport streams. The
//! TrueHD access units are those of a short real stream. The real files are tested in `samples`.

mod support;

use symphonia_common::mpeg::surround::{Ac3Header, DcaHeader};
use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia_core::packet::Packet;
use symphonia_core::units::Timestamp;

use support::*;

/// The TrueHD access units (the first 32, with the major sync of the first) of a 5.1 stream.
const TRUEHD_AUS: &[u8] = include_bytes!("data/truehd_5_1_32au.thd");

const HDMV: &[u8] = &[0x05, 0x04, b'H', b'D', b'M', b'V'];

/// Pack `(value, bits)` fields, MSB first, into bytes.
fn pack(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut bits = vec![];

    for &(v, n) in fields {
        bits.extend((0..n).rev().map(|i| (v >> i) & 1 != 0));
    }

    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0u8, |b, (i, &bit)| b | u8::from(bit) << (7 - i)))
        .collect()
}

/// AC-3 frames of 48 kHz stereo, 768 bytes and 1536 samples. The payload is filled with the
/// frame index.
fn ac3_frames(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            // Sync word, CRC 1, fscod 0, frmsizecod 20 (192 kbps), bsid 8, bsmod 0, acmod 2.
            let mut f = vec![0x0b, 0x77, 0, 0, 20, 8 << 3, 2 << 5, 0];
            let hdr = Ac3Header::parse(&f).unwrap();
            assert_eq!((hdr.frame_size, hdr.sample_rate), (768, 48_000));
            f.resize(hdr.frame_size, i as u8);
            f
        })
        .collect()
}

/// E-AC-3 frames of 48 kHz stereo with 6 blocks, 768 bytes and 1536 samples.
fn eac3_frames(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            // Sync word, independent substream 0 with a frame size of 768 bytes, fscod 0, 6
            // blocks, acmod 2, no LFE, bsid 16.
            let mut f = vec![0x0b, 0x77, 0x01, 0x7f, 0b0011_0100, 16 << 3, 0, 0];
            let hdr = Ac3Header::parse(&f).unwrap();
            assert_eq!((hdr.frame_size, hdr.samples), (768, 1536));
            f.resize(hdr.frame_size, i as u8);
            f
        })
        .collect()
}

/// DTS core frames of 48 kHz stereo, 16 blocks of 32 samples, and 512 bytes.
fn dca_frames(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut f = pack(&[
                (0x7ffe_8001, 32),
                // Normal frame, 32 samples, no CRC, 16 blocks, frame size 512.
                (1, 1),
                (31, 5),
                (0, 1),
                (15, 7),
                (511, 14),
                // Stereo, 48 kHz, 4 Mbps, then the flags.
                (2, 6),
                (13, 4),
                (5, 5),
                (0, 10),
                // No LFE.
                (0, 2),
            ]);
            f.resize(512, 0x40 + (i % 8) as u8);
            let hdr = DcaHeader::parse(&f).unwrap();
            assert_eq!((hdr.frame_size, hdr.samples, hdr.sample_rate), (512, 512, 48_000));
            f
        })
        .collect()
}

/// The access units of the TrueHD fixture.
fn truehd_aus() -> Vec<&'static [u8]> {
    let mut aus = vec![];
    let mut pos = 0;

    while pos < TRUEHD_AUS.len() {
        let len = (usize::from(TRUEHD_AUS[pos] & 0xf) << 8 | usize::from(TRUEHD_AUS[pos + 1])) * 2;
        aus.push(&TRUEHD_AUS[pos..pos + len]);
        pos += len;
    }

    assert_eq!(pos, TRUEHD_AUS.len());
    aus
}

fn spec(stream_type: u8, descriptors: &[u8]) -> StreamSpec {
    StreamSpec { stream_type, pid: 0x101, descriptors: descriptors.to_vec() }
}

fn mux_stream(
    stream_type: u8,
    descriptors: &[u8],
    program_descriptors: &[u8],
    frames: &[Vec<u8>],
    samples: u64,
) -> Vec<u8> {
    let mut mux = TsMux::new(188);
    mux.psi_with_program_descriptors(
        0x1000,
        program_descriptors,
        &[spec(stream_type, descriptors)],
    );

    for (n, group) in frames.chunks(3).enumerate() {
        let pts = 1_000_000 + (n * 3) as u64 * samples * 90_000 / 48_000;
        mux.pes(0x101, 0xbd, Some(pts), &group.concat());
    }

    mux.out
}

/// Check that the stream of `frames` is a single track of the codec `codec`, that the packets are
/// the frames, and that the timeline is that of the frames, then check seeking.
fn check_stream(data: Vec<u8>, codec: AudioCodecId, frames: &[Vec<u8>], samples: i64) {
    let mut reader = open(data);
    assert_eq!(reader.tracks().len(), 1);

    let track = reader.tracks()[0].clone();
    assert_eq!(track.id, 0x101);
    assert_eq!(track.time_base.unwrap().denom.get(), 48_000);
    assert_eq!(track.start_ts.get(), 0);
    assert_eq!(track.duration.unwrap().get() as i64, frames.len() as i64 * samples);

    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, codec);
    assert_eq!(params.sample_rate, Some(48_000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), frames.len());

    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * samples);
        assert_eq!(p.dur.get() as i64, samples);
        assert_eq!(p.data[..], frames[i][..]);
    }

    // Seek forward, backward, and to the ends.
    let n = frames.len() as i64;

    for target in [0, 1, 7 * samples, 7 * samples + 5, 2 * samples, (n - 1) * samples, n * samples]
    {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0x101 },
            )
            .unwrap();

        assert_eq!(seeked.required_ts.get(), target);
        assert!(seeked.actual_ts.get() <= target);

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts, seeked.actual_ts);
        assert!(packet.pts.get() + samples > target - samples, "target {target}");

        // The packets continue contiguously, and are the frames.
        let first = (packet.pts.get() / samples) as usize;
        assert_eq!(packet.data[..], frames[first][..]);
        let next = reader.next_packet().unwrap();
        assert_eq!(next.is_some(), first + 1 < frames.len());
    }
}

#[test]
fn ac3_atsc_and_bluray_stream_types() {
    let frames = ac3_frames(40);

    // ATSC, with the AC-3 registration and descriptor of the muxers, and without.
    for descriptors in [&[0x05, 0x04, b'A', b'C', b'-', b'3', 0x6a, 0x01, 0x00][..], &[]] {
        check_stream(
            mux_stream(0x81, descriptors, &[], &frames, 1536),
            CODEC_ID_AC3,
            &frames,
            1536,
        );
    }

    // Blu-ray.
    check_stream(mux_stream(0x81, &[], HDMV, &frames, 1536), CODEC_ID_AC3, &frames, 1536);
}

#[test]
fn eac3_atsc_and_bluray_stream_types() {
    let frames = eac3_frames(40);

    check_stream(
        mux_stream(
            0x87,
            &[0x05, 0x04, b'E', b'A', b'C', b'3', 0x7a, 0x01, 0x00],
            &[],
            &frames,
            1536,
        ),
        CODEC_ID_EAC3,
        &frames,
        1536,
    );

    // The Blu-ray primary and secondary E-AC-3.
    for stream_type in [0x84, 0xa1] {
        check_stream(
            mux_stream(stream_type, &[], HDMV, &frames, 1536),
            CODEC_ID_EAC3,
            &frames,
            1536,
        );
        check_stream(
            mux_stream(stream_type, &[], b"\x05\x04HDPR", &frames, 1536),
            CODEC_ID_EAC3,
            &frames,
            1536,
        );
    }
}

#[test]
fn bluray_only_stream_types_need_the_registration() {
    let eac3 = eac3_frames(20);
    let dca = dca_frames(20);

    for stream_type in [0x84, 0xa1] {
        let mut mux = TsMux::new(188);
        mux.psi(0x1000, &[spec(stream_type, &[])]);
        mux.pes(0x101, 0xbd, Some(90_000), &eac3.concat());
        assert!(
            symphonia_format_mpegts::MpegTsReader::try_new(mss(mux.out), Default::default())
                .is_err()
        );
    }

    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[spec(0xa2, &[])]);
    mux.pes(0x101, 0xbd, Some(90_000), &dca.concat());
    assert!(
        symphonia_format_mpegts::MpegTsReader::try_new(mss(mux.out), Default::default()).is_err()
    );
}

#[test]
fn dca_stream_types() {
    let frames = dca_frames(60);

    // DTS, DTS-HD, DTS-HD Master Audio, ATSC DTS, with and without the Blu-ray registration.
    for stream_type in [0x82, 0x85, 0x86, 0x8a] {
        for program_descriptors in [&[][..], HDMV] {
            check_stream(
                mux_stream(stream_type, &[], program_descriptors, &frames, 512),
                CODEC_ID_DCA,
                &frames,
                512,
            );
        }
    }

    // The Blu-ray secondary DTS Express.
    check_stream(mux_stream(0xa2, &[], HDMV, &frames, 512), CODEC_ID_DCA, &frames, 512);
}

#[test]
fn private_data_is_identified_by_descriptors() {
    let ac3 = ac3_frames(30);
    let eac3 = eac3_frames(30);
    let dca = dca_frames(30);

    // The DVB descriptors.
    check_stream(mux_stream(0x06, &[0x6a, 0x01, 0x00], &[], &ac3, 1536), CODEC_ID_AC3, &ac3, 1536);
    check_stream(
        mux_stream(0x06, &[0x7a, 0x01, 0x00], &[], &eac3, 1536),
        CODEC_ID_EAC3,
        &eac3,
        1536,
    );
    check_stream(mux_stream(0x06, &[0x7b, 0x01, 0x00], &[], &dca, 512), CODEC_ID_DCA, &dca, 512);

    // The registration descriptors.
    for (tag, frames, codec, samples) in [
        (b"AC-3", &ac3, CODEC_ID_AC3, 1536),
        (b"EAC3", &eac3, CODEC_ID_EAC3, 1536),
        (b"DTS1", &dca, CODEC_ID_DCA, 512),
        (b"DTS2", &dca, CODEC_ID_DCA, 512),
        (b"DTS3", &dca, CODEC_ID_DCA, 512),
    ] {
        let mut desc = vec![0x05, 0x04];
        desc.extend_from_slice(tag);
        check_stream(mux_stream(0x06, &desc, &[], frames, samples), codec, frames, samples as i64);
    }

    // A user private stream type with a registration descriptor.
    check_stream(
        mux_stream(0x91, &[0x05, 0x04, b'A', b'C', b'-', b'3'], &[], &ac3, 1536),
        CODEC_ID_AC3,
        &ac3,
        1536,
    );
}

/// Multiplex a Blu-ray TrueHD PID: `reps` repetitions of the TrueHD access units with the stream
/// ID extension 0x72, interleaved with AC-3 frames with the stream ID extension 0x76.
fn mux_truehd(reps: usize, ac3: &[Vec<u8>], pid: u16, packet_size: usize) -> Vec<u8> {
    let aus = truehd_aus();
    let mut mux = TsMux::new(packet_size);
    mux.psi_with_program_descriptors(
        0x1000,
        HDMV,
        &[StreamSpec { stream_type: 0x83, pid, descriptors: vec![] }],
    );

    // 32 access units of 40 samples: 1280 samples, 2400 ticks.
    for rep in 0..reps {
        let pts = 1_000_000 + rep as u64 * 2400;
        mux.pes_with_extension(pid, 0xfd, Some(pts), Some(0x72), &aus.concat());

        if let Some(frame) = ac3.get(rep) {
            // An AC-3 frame lasts 2880 ticks.
            let pts = 1_000_000 + rep as u64 * 2880;
            mux.pes_with_extension(pid, 0xfd, Some(pts), Some(0x76), frame);
        }
    }

    mux.out
}

#[test]
fn bluray_truehd_has_an_embedded_ac3_track() {
    let reps = 8;
    let ac3 = ac3_frames(reps);
    let aus = truehd_aus();
    assert_eq!(aus.len(), 32);

    let mut reader = open(mux_truehd(reps, &ac3, 0x1100, 192));
    assert_eq!(reader.tracks().len(), 2);

    let (thd, emb) = (reader.tracks()[0].clone(), reader.tracks()[1].clone());
    assert_eq!((thd.id, emb.id), (0x1100, 0x1100 + 0x2000));
    assert_eq!(audio_params(&reader, 0).codec, CODEC_ID_TRUEHD);
    assert_eq!(audio_params(&reader, 1).codec, CODEC_ID_AC3);
    assert_eq!(audio_params(&reader, 0).channels.as_ref().unwrap().count(), 6);
    assert_eq!(audio_params(&reader, 1).channels.as_ref().unwrap().count(), 2);
    assert_eq!((thd.start_ts.get(), emb.start_ts.get()), (0, 0));
    assert_eq!(thd.duration.unwrap().get(), reps as u64 * 1280);
    assert_eq!(emb.duration.unwrap().get(), reps as u64 * 1536);

    let packets = read_all(&mut reader);
    let thd_packets: Vec<&Packet> = packets.iter().filter(|p| p.track_id == thd.id).collect();
    let emb_packets: Vec<&Packet> = packets.iter().filter(|p| p.track_id == emb.id).collect();

    assert_eq!(thd_packets.len(), reps * 32);
    assert_eq!(emb_packets.len(), reps);

    for (i, p) in thd_packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * 40);
        assert_eq!(p.dur.get(), 40);
        assert_eq!(p.data[..], aus[i % 32][..]);
    }

    for (i, p) in emb_packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * 1536);
        assert_eq!(p.dur.get(), 1536);
        assert_eq!(p.data[..], ac3[i][..]);
    }

    // Seek both tracks.
    for (track, samples, n) in [(&emb, 1536, reps as i64), (&thd, 40, reps as i64 * 32)] {
        for target in [0, 1, 3 * samples + 7, n * samples / 2, 2 * samples, (n - 1) * samples] {
            let seeked = reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp { ts: Timestamp::new(target), track_id: track.id },
                )
                .unwrap();

            assert!(seeked.actual_ts.get() <= target);

            // The next packet of the track is the one seeked to, and the packets are of the track
            // alone.
            let p = loop {
                let p = reader.next_packet().unwrap().unwrap();
                if p.track_id == track.id {
                    break p;
                }
            };

            assert_eq!(p.pts, seeked.actual_ts);
            assert!(p.pts.get() + samples > target - 128 * 40 - samples);
        }
    }
}

#[test]
fn truehd_without_an_embedded_ac3_stream_is_a_single_track() {
    // Without the Blu-ray registration, as written by FFmpeg.
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x83, pid: 0x100, descriptors: vec![] }]);

    let aus = truehd_aus();

    for rep in 0..4 {
        mux.pes(0x100, 0xbd, Some(90_000 + rep * 2400), &aus.concat());
    }

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(reader.tracks()[0].id, 0x100);
    assert_eq!(audio_params(&reader, 0).codec, CODEC_ID_TRUEHD);
    assert_eq!(read_all(&mut reader).len(), 4 * 32);
}

#[test]
fn truehd_pes_without_extension_are_not_the_embedded_stream() {
    // An extension with another stream ID extension than 0x76, and the stream ID of an extension
    // (0xfd) without PES extension, are TrueHD.
    let aus = truehd_aus();
    let mut mux = TsMux::new(188);
    mux.psi_with_program_descriptors(
        0x1000,
        HDMV,
        &[StreamSpec { stream_type: 0x83, pid: 0x1100, descriptors: vec![] }],
    );

    mux.pes_with_extension(0x1100, 0xfd, Some(90_000), Some(0x72), &aus[..16].concat());
    mux.pes_with_extension(0x1100, 0xfd, Some(90_000 + 1200), None, &aus[16..].concat());

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(read_all(&mut reader).len(), 32);
}
