// Symphonia Musepack seek tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Seeking regression tests.
//!
//! The invariant checked throughout: after `seek(target)`, the audio the reader/decoder pair
//! delivers is *bit-identical* to what a linear decode from the start delivers from the position
//! reported by `SeekedTo::actual_ts` on. (SV7 scale factors are delta-coded across frames and the
//! synthesis filter has memory, so this fails if either is not recovered at the seek point.)
//!
//! The SV7 stream is synthetic (random frame payloads in a valid container: decoding it is
//! deterministic, which is all that is needed to compare two decode paths), the SV8 streams are
//! the committed fixtures, and an optional real-world SV7 file can be given in
//! `MUSEPACK_SEEK_SAMPLE`.

use std::io::Cursor;
use std::path::Path;

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

const FRAME: u64 = 1152;
const SYNTH_DELAY: u64 = 481;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Stream {
    reader: MpcReader<'static>,
    decoder: MpcDecoder,
    /// Track length in sample-frames.
    len: u64,
    channels: usize,
}

fn open(bytes: &[u8]) -> Stream {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = &reader.tracks()[0];
    let len = track.num_frames.expect("track length");
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let channels = params.channels.as_ref().unwrap().count();
    let decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    Stream { reader, decoder, len, channels }
}

impl Stream {
    /// Decodes up to `max_frames` sample-frames from the current position, interleaved.
    fn read(&mut self, max_frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        while out.len() < max_frames * self.channels {
            let Some(packet) = self.reader.next_packet().unwrap()
            else {
                break;
            };
            let GenericAudioBufferRef::F32(buf) = self.decoder.decode(&packet).unwrap()
            else {
                panic!("not f32");
            };
            for i in 0..buf.frames() {
                for ch in 0..self.channels {
                    out.push(buf.plane(ch).unwrap()[i]);
                }
            }
        }
        out.truncate(max_frames * self.channels);
        out
    }

    fn seek(&mut self, mode: SeekMode, target: u64) -> symphonia_core::errors::Result<u64> {
        let seeked = self
            .reader
            .seek(mode, SeekTo::Timestamp { ts: Timestamp::new(target as i64), track_id: 0 })?;
        self.decoder.reset();
        assert_eq!(seeked.required_ts.get(), target as i64);
        Ok(seeked.actual_ts.get() as u64)
    }
}

fn same(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-6 || x.to_bits() == y.to_bits())
}

/// Seeks every `targets` entry with both modes, on one long-lived reader/decoder pair (so seeks
/// start from arbitrary earlier positions, forwards and backwards), and checks the result
/// against `linear`.
fn check_seeks(bytes: &[u8], targets: &[u64], block_samples: u64) {
    let mut lin = open(bytes);
    let linear = lin.read(usize::MAX / 4);
    assert_eq!(linear.len() as u64, lin.len * lin.channels as u64, "linear length == track length");
    let ch = lin.channels;
    let len = lin.len;

    let mut s = open(bytes);
    for &target in targets {
        for mode in [SeekMode::Accurate, SeekMode::Coarse] {
            // Read a few packets' worth, and also enough to run into the end of the stream.
            let want = (3 * block_samples) as usize + 100;
            let actual = s.seek(mode, target).unwrap_or_else(|e| panic!("seek {target}: {e}"));
            match mode {
                SeekMode::Accurate => assert_eq!(actual, target, "accurate seek lands on target"),
                SeekMode::Coarse => {
                    assert!(actual <= target, "coarse seek {target} landed after: {actual}");
                    assert!(
                        // (The final packet is the landing place for seeks to the very end.)
                        target - actual < block_samples || target == len,
                        "coarse seek {target} landed too early: {actual}"
                    );
                }
            }
            let got = s.read(want);
            let from = actual as usize * ch;
            let expect = &linear[from..(from + want * ch).min(linear.len())];
            assert!(
                same(&got, expect),
                "{mode:?} seek to {target} (landed {actual}, len {len}): audio differs from linear \
                 decode (got {} / expected {} samples, first diff at {:?})",
                got.len(),
                expect.len(),
                got.iter().zip(expect).position(|(a, b)| (a - b).abs() > 1e-6),
            );
        }
    }
}

/// Targets covering the interesting corners of a stream of `len` samples with `block` samples per
/// packet.
fn targets_for(len: u64, block: u64) -> Vec<u64> {
    let mut t = vec![
        0,
        1,
        SYNTH_DELAY - 1,
        SYNTH_DELAY,
        SYNTH_DELAY + 1,
        block - SYNTH_DELAY - 1,
        block - SYNTH_DELAY,
        block - 1,
        block,
        block + 1,
        2 * block,
        2 * block + SYNTH_DELAY,
        len / 3,
        len / 2 + 7,
        len.saturating_sub(3 * block),
        len.saturating_sub(block + 1),
        len.saturating_sub(block),
        len.saturating_sub(100),
        len.saturating_sub(1),
        len,
    ];
    // A deterministic spread, visited in a shuffled order (so a good share are backwards seeks).
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..25 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t.push(x % (len + 1));
    }
    t.retain(|&v| v <= len);
    t
}

// ---------------------------------------------------------------------------------------------
// Synthetic SV7 stream
// ---------------------------------------------------------------------------------------------

struct BitWriter {
    bytes: Vec<u8>,
    bits: u64,
}

impl BitWriter {
    fn put(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            if (value >> i) & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// Builds an SV7 file of `frames` frames of pseudo-random payload. The payload decodes to
/// *something* deterministic; what matters is that frame content, lengths and scale-factor
/// deltas are irregular, as in a real stream.
fn synthetic_sv7(frames: u32, last_frame_samples: u32, seed: u64) -> Vec<u8> {
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut w = BitWriter { bytes: Vec::new(), bits: 0 };
    w.put(u64::from(frames), 32);
    w.put(0, 1); // intensity stereo
    w.put(1, 1); // mid/side
    w.put(20, 6); // max band
    w.put(0, 4); // profile
    w.put(0, 2); // link
    w.put(0, 2); // 44100 Hz
    w.put(0, 16); // estimated peak
    for _ in 0..4 {
        w.put(0, 16); // replay gain
    }
    w.put(1, 1); // true gapless
    w.put(u64::from(last_frame_samples), 11);
    w.put(0, 1); // fast seek
    w.put(0, 19);
    w.put(0, 8); // encoder version
    assert_eq!(w.bits, 168);

    for _ in 0..frames {
        let len = 200 + next() % 1800;
        w.put(len, 20);
        for _ in 0..len {
            w.put(next() >> 40 & 1, 1);
        }
    }
    // Pad to a whole number of 32-bit words, then store them little-endian.
    while w.bytes.len() % 4 != 0 {
        w.bytes.push(0);
    }
    let mut file = b"MP+\x07".to_vec();
    for word in w.bytes.chunks_exact(4) {
        file.extend([word[3], word[2], word[1], word[0]]);
    }
    file
}

fn sv7_len(frames: u32, last: u32) -> u64 {
    let max = u64::from(frames) * FRAME - SYNTH_DELAY;
    (u64::from(frames) * FRAME - (FRAME - u64::from(last))).min(max)
}

#[test]
fn sv7_synthetic_packets_describe_the_stream() {
    for last in [1, 300, 671, 700, 1152] {
        let bytes = synthetic_sv7(40, last, 7);
        let mut s = open(&bytes);
        assert_eq!(s.len, sv7_len(40, last));

        let mut total = 0;
        let mut n = 0u64;
        while let Some(p) = s.reader.next_packet().unwrap() {
            assert_eq!(p.pts.get(), (n * FRAME) as i64 - SYNTH_DELAY as i64, "pts of packet {n}");
            assert_eq!(p.block_dur().get(), FRAME);
            let GenericAudioBufferRef::F32(buf) = s.decoder.decode(&p).unwrap()
            else {
                panic!()
            };
            assert_eq!(buf.frames() as u64, p.dur.get(), "packet {n} output matches its duration");
            total += p.dur.get();
            n += 1;
        }
        assert_eq!(n, 40);
        assert_eq!(total, s.len, "durations add up to the track length (last={last})");
    }
}

#[test]
fn sv7_synthetic_seek_matches_linear_decode() {
    for (frames, last) in [(150u32, 1152u32), (150, 300), (150, 800), (9, 100), (3, 1152), (1, 600)]
    {
        let bytes = synthetic_sv7(frames, last, 0xC0FFEE + u64::from(frames));
        let len = sv7_len(frames, last);
        check_seeks(
            &bytes,
            &targets_for(len, FRAME).into_iter().filter(|&t| t <= len).collect::<Vec<_>>(),
            FRAME,
        );
    }
}

#[test]
fn seek_past_the_end_is_out_of_range_and_recoverable() {
    let bytes = synthetic_sv7(30, 900, 3);
    let mut s = open(&bytes);
    let len = s.len;
    let _ = s.read(5000);

    for target in [len + 1, len + 100_000] {
        match s.seek(SeekMode::Accurate, target) {
            Err(Error::SeekError(SeekErrorKind::OutOfRange)) => (),
            other => panic!("seek to {target} (len {len}): {other:?}"),
        }
    }
    // The reader is still usable afterwards.
    assert_eq!(s.seek(SeekMode::Accurate, 10_000).unwrap(), 10_000);
    assert!(!s.read(100).is_empty());

    // Seeking to exactly the end is allowed and yields nothing.
    assert_eq!(s.seek(SeekMode::Accurate, len).unwrap(), len);
    assert!(s.read(100).is_empty());

    // Negative timestamps clamp to the start.
    let seeked = s
        .reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(-5), track_id: 0 })
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), 0);
}

// ---------------------------------------------------------------------------------------------
// Synthetic SV8 stream
// ---------------------------------------------------------------------------------------------

fn varint(mut v: u64) -> Vec<u8> {
    let mut groups = vec![(v & 0x7F) as u8];
    v >>= 7;
    while v != 0 {
        groups.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    groups.reverse();
    groups
}

/// A block: two-letter key, a size field counting the whole block, then the payload.
fn block(key: &[u8; 2], payload: &[u8]) -> Vec<u8> {
    let mut n = 1;
    let size = loop {
        let size = (2 + n + payload.len()) as u64;
        if varint(size).len() == n {
            break size;
        }
        n += 1;
    };
    let mut out = key.to_vec();
    out.extend(varint(size));
    out.extend(payload);
    out
}

/// Builds an SV8 file of `packets` `AP` blocks (`2^block_pwr` frames each, the last one short)
/// of pseudo-random payload. The stream claims `encoder_pns` noise substitution, which the random
/// payloads then use at will (`Res == -1`).
fn synthetic_sv8(
    block_pwr: u32,
    beg_silence: u64,
    packets: u64,
    pns_flag: Option<bool>,
) -> Vec<u8> {
    let mut rng = 0x1234_5678_9ABC_DEF1u64 ^ (packets << 8) ^ u64::from(block_pwr);
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let block_frames = 1u64 << block_pwr;
    let frames = (packets - 1) * block_frames + block_frames.min(3);
    let samples = frames * FRAME - SYNTH_DELAY - 100;

    let mut sh = BitWriter { bytes: Vec::new(), bits: 0 };
    sh.put(0, 32); // crc (not checked)
    sh.put(8, 8); // stream version
    for b in varint(samples).into_iter().chain(varint(beg_silence)) {
        sh.put(u64::from(b), 8);
    }
    sh.put(0, 3); // 44100 Hz
    sh.put(20 - 1, 5); // max band
    sh.put(2 - 1, 4); // channels
    sh.put(1, 1); // mid/side
    sh.put(u64::from(block_pwr / 2), 3);

    let mut file = b"MPCK".to_vec();
    file.extend(block(b"SH", &sh.bytes));
    if let Some(pns) = pns_flag {
        let mut ei = BitWriter { bytes: Vec::new(), bits: 0 };
        ei.put(0, 7);
        ei.put(u64::from(pns), 1);
        ei.put(1, 8);
        ei.put(2, 8);
        ei.put(3, 8);
        file.extend(block(b"EI", &ei.bytes));
    }
    for _ in 0..packets {
        let len = 300 + next() % 2500;
        let payload: Vec<u8> = (0..len).map(|_| (next() >> 33) as u8).collect();
        file.extend(block(b"AP", &payload));
    }
    file.extend(block(b"SE", &[]));
    file
}

#[test]
fn sv8_synthetic_packets_describe_the_stream() {
    for (pwr, beg) in [(0, 0), (2, 0), (2, 700), (4, 5000)] {
        let bytes = synthetic_sv8(pwr, beg, 6, None);
        let mut s = open(&bytes);
        let skip = SYNTH_DELAY + beg;
        let mut total = 0;
        let mut n = 0u64;
        while let Some(p) = s.reader.next_packet().unwrap() {
            assert_eq!(p.pts.get(), (n * (FRAME << pwr)) as i64 - skip as i64, "pts of packet {n}");
            let GenericAudioBufferRef::F32(buf) = s.decoder.decode(&p).unwrap()
            else {
                panic!()
            };
            assert_eq!(buf.frames() as u64, p.dur.get(), "packet {n} output matches its duration");
            total += p.dur.get();
            n += 1;
        }
        assert_eq!(n, 6, "every packet is delivered (pwr {pwr})");
        assert_eq!(total, s.len, "durations add up to the track length (pwr {pwr}, beg {beg})");
    }
}

#[test]
fn sv8_synthetic_seek_matches_linear_decode() {
    // The payloads are random, so noise substitution (whose generator state runs on from packet
    // to packet) is in use throughout. Without an `EI` block, or with one that allows it, the
    // demuxer has to recover that state; with one that rules it out it must not need to.
    for (pwr, beg, packets, pns) in [
        (0, 0, 40, None),
        (2, 0, 14, None),
        (2, 700, 14, Some(true)),
        (4, 5000, 5, None),
        (4, 0, 1, None),
        (6, 0, 3, Some(true)),
    ] {
        let bytes = synthetic_sv8(pwr, beg, packets, pns);
        let probe = open(&bytes);
        let b = FRAME << pwr;
        check_seeks(&bytes, &targets_for(probe.len, b), b);
    }
}

// ---------------------------------------------------------------------------------------------
// SV8 fixtures
// ---------------------------------------------------------------------------------------------

#[test]
fn sv8_fixtures_seek_matches_linear_decode() {
    for name in ["fixture_mono_q2.mpc", "fixture_stereo_q5.mpc"] {
        let bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name))
            .expect("fixture");
        let probe = open(&bytes);
        let block = probe.reader.tracks()[0]
            .codec_params
            .as_ref()
            .map(|p| match p {
                CodecParameters::Audio(a) => a.max_frames_per_packet.unwrap(),
                _ => unreachable!(),
            })
            .unwrap();
        check_seeks(&bytes, &targets_for(probe.len, block), block);
    }
}

#[test]
fn sv8_packets_describe_the_stream() {
    let bytes =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_mono_q2.mpc"))
            .unwrap();
    let mut s = open(&bytes);
    let mut total = 0;
    let mut expected_pts = None;
    while let Some(p) = s.reader.next_packet().unwrap() {
        if let Some(e) = expected_pts {
            assert_eq!(p.pts.get(), e, "packets are contiguous");
        }
        expected_pts = Some(p.pts.get() + p.block_dur().get() as i64);
        let GenericAudioBufferRef::F32(buf) = s.decoder.decode(&p).unwrap()
        else {
            panic!()
        };
        assert_eq!(buf.frames() as u64, p.dur.get());
        total += p.dur.get();
    }
    assert_eq!(total, s.len);
}

// ---------------------------------------------------------------------------------------------
// Real-world file (optional)
// ---------------------------------------------------------------------------------------------

/// Any real Musepack file (SV7 or SV8), given by `MUSEPACK_SEEK_SAMPLE`; skipped when unset or
/// absent. A handful of seeks is enough: they all decode from the seek point to the next packets
/// only, but the reference decode is of the whole file.
#[test]
fn real_file_seek_matches_linear_decode() {
    let Ok(path) = std::env::var("MUSEPACK_SEEK_SAMPLE")
    else {
        eprintln!("skipping: MUSEPACK_SEEK_SAMPLE not set");
        return;
    };
    let Ok(bytes) = std::fs::read(&path)
    else {
        eprintln!("skipping: {path} not readable");
        return;
    };
    let probe = open(&bytes);
    let block = probe.reader.tracks()[0]
        .codec_params
        .as_ref()
        .map(|p| match p {
            CodecParameters::Audio(a) => a.max_frames_per_packet.unwrap(),
            _ => unreachable!(),
        })
        .unwrap();
    let len = probe.len;
    let sr = 44_100;
    let mut targets = vec![0, 5_000, sr * 10, sr * 60 + 123, len / 2, len - 2 * block, len - 1];
    targets.retain(|&t| t < len);
    // Backwards seeks too.
    targets.extend([len / 4, 777, sr * 30]);
    targets.retain(|&t| t < len);
    check_seeks(&bytes, &targets, block);
}
