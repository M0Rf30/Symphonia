// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! End-to-end tests of the SBC, aptX and aptX HD readers and decoders.
//!
//! The decoders are integer decoders, so their output is compared with FFmpeg's decoders bit for
//! bit. The small fixtures in `tests/data` were encoded with FFmpeg's `sbc`, `aptx` and `aptx_hd`
//! encoders, and their decoded samples are pinned by hash (verified equal to FFmpeg's output
//! when they were made). The tests that run `ffmpeg` are skipped when it is not on the `PATH`.

use std::io::Cursor;
use std::process::{Command, Stdio};

use symphonia_codec_aptx::AptxDecoder;
use symphonia_codec_lc3::Lc3Decoder;
use symphonia_codec_sbc::SbcDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SBC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_bt::{AptxHdReader, AptxReader, Lc3Reader, SbcReader};
use symphonia_testutil::{Fnv, XorShift64};

const SBC_STEREO: &[u8] = include_bytes!("data/stereo_44k_joint.sbc");
const SBC_STEREO_4BLK: &[u8] = include_bytes!("data/stereo_48k_4blk.sbc");
const SBC_MONO: &[u8] = include_bytes!("data/mono_16k.sbc");
const MSBC: &[u8] = include_bytes!("data/msbc.msbc");
const APTX: &[u8] = include_bytes!("data/stereo_48k.aptx");
const APTX_HD: &[u8] = include_bytes!("data/stereo_44k.aptxhd");
const LC3_MONO: &[u8] = include_bytes!("data/mono_16k_10ms.lc3");
const LC3_MONO_REF: &[u8] = include_bytes!("data/mono_16k_10ms.ref.wav");
const LC3_STEREO: &[u8] = include_bytes!("data/stereo_24k_5ms.lc3");
const LC3_STEREO_REF: &[u8] = include_bytes!("data/stereo_24k_5ms.ref.wav");

fn mss(data: &[u8]) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default())
}

/// What a stream decodes to.
struct Decoded {
    rate: u32,
    channels: usize,
    packets: usize,
    samples: Vec<i32>,
}

/// Decodes every packet of `reader` to interleaved samples: 16-bit for SBC, and the 24-bit samples
/// shifted to 32 bits for aptX.
fn decode(mut reader: Box<dyn FormatReader>) -> Decoded {
    let track = reader.tracks()[0].clone();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };

    let mut dec: Box<dyn AudioDecoder> = new_decoder(params);

    let mut samples = Vec::new();
    let mut packets = 0;
    let mut rate = 0;
    let mut channels = 0;

    while let Some(packet) = reader.next_packet().unwrap() {
        packets += 1;
        let buf = dec.decode(&packet).unwrap();
        rate = buf.spec().rate();
        channels = buf.spec().channels().count();
        if params.codec == CODEC_ID_SBC {
            // 16-bit samples, which are not scaled to 32 bits.
            let mut tmp = Vec::new();
            buf.copy_to_vec_interleaved::<i16>(&mut tmp);
            samples.extend(tmp.iter().map(|&s| i32::from(s)));
        }
        else {
            let mut tmp = Vec::new();
            buf.copy_to_vec_interleaved::<i32>(&mut tmp);
            samples.extend_from_slice(&tmp);
        }
    }

    Decoded { rate, channels, packets, samples }
}

fn new_decoder(params: &AudioCodecParameters) -> Box<dyn AudioDecoder> {
    if params.codec == CODEC_ID_SBC {
        Box::new(SbcDecoder::try_new(params, &Default::default()).unwrap())
    }
    else {
        Box::new(AptxDecoder::try_new(params, &Default::default()).unwrap())
    }
}

fn fnv(samples: &[i32]) -> u64 {
    let mut h = Fnv::new();
    for &s in samples {
        h.add_u32(s as u32);
    }
    h.finish()
}

fn sbc_reader(data: &[u8]) -> Box<dyn FormatReader> {
    Box::new(SbcReader::try_new(mss(data), FormatOptions::default()).unwrap())
}

/// Decodes `data` with FFmpeg. `format` is the demuxer, and `pcm` the raw output format. Returns
/// None if FFmpeg is not available.
fn ffmpeg_decode(data: &[u8], format: &str, pcm: &str) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("symphonia-bt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;

    let mut hasher = Fnv::new();
    hasher.write(data);
    hasher.write(format.as_bytes());
    let path = dir.join(format!("{:016x}.bin", hasher.finish()));
    std::fs::write(&path, data).ok()?;

    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", format, "-i"])
        .arg(&path)
        .args(["-f", pcm, "-"])
        .stderr(Stdio::null())
        .output()
        .ok()?;

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);

    out.status.success().then_some(out.stdout)
}

fn to_s16le(samples: &[i32]) -> Vec<u8> {
    samples.iter().flat_map(|&s| (s as i16).to_le_bytes()).collect()
}

fn to_s32le(samples: &[i32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Print the hashes of the fixtures, to regenerate the expected values.
fn check_hash(name: &str, samples: &[i32], expected: u64) {
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        println!("{name}: {:#018x}", fnv(samples));
    }
    else {
        assert_eq!(fnv(samples), expected, "{name}");
    }
}

#[test]
fn sbc_stereo_joint_44k() {
    let d = decode(sbc_reader(SBC_STEREO));
    assert_eq!((d.rate, d.channels), (44100, 2));
    check_hash("sbc stereo", &d.samples, 0xf3be0646f6e08250);

    if let Some(reference) = ffmpeg_decode(SBC_STEREO, "sbc", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn sbc_stereo_four_blocks_48k() {
    let d = decode(sbc_reader(SBC_STEREO_4BLK));
    assert_eq!((d.rate, d.channels), (48000, 2));
    check_hash("sbc 4 blocks", &d.samples, 0x088f79c7e0442166);

    if let Some(reference) = ffmpeg_decode(SBC_STEREO_4BLK, "sbc", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn sbc_mono_16k() {
    let d = decode(sbc_reader(SBC_MONO));
    assert_eq!((d.rate, d.channels), (16000, 1));
    check_hash("sbc mono", &d.samples, 0x67670235da299474);

    if let Some(reference) = ffmpeg_decode(SBC_MONO, "sbc", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn msbc() {
    let d = decode(sbc_reader(MSBC));
    assert_eq!((d.rate, d.channels), (16000, 1));
    // 120 samples per frame of 57 bytes.
    assert_eq!(d.samples.len(), MSBC.len() / 57 * 120);
    check_hash("msbc", &d.samples, 0x7bbada8ac12f82a1);

    if let Some(reference) = ffmpeg_decode(MSBC, "sbc", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn aptx() {
    let reader = AptxReader::try_new(mss(APTX), FormatOptions::default()).unwrap();
    let d = decode(Box::new(reader));
    assert_eq!((d.rate, d.channels), (48000, 2));
    // 4 samples per 4 bytes, and 256 blocks per packet.
    assert_eq!(d.samples.len(), APTX.len() / 4 * 4 * 2);
    assert_eq!(d.packets, APTX.len().div_ceil(1024));
    check_hash("aptx", &d.samples, 0xe3afd41c23d176aa);

    if let Some(reference) = ffmpeg_decode(APTX, "aptx", "s32le") {
        assert_eq!(to_s32le(&d.samples), reference);
    }
}

#[test]
fn aptx_hd() {
    let reader = AptxHdReader::try_new(mss(APTX_HD), FormatOptions::default()).unwrap();
    let d = decode(Box::new(reader));
    assert_eq!((d.rate, d.channels), (48000, 2));
    assert_eq!(d.samples.len(), APTX_HD.len() / 6 * 4 * 2);
    check_hash("aptx hd", &d.samples, 0x86129a56a3265100);

    if let Some(reference) = ffmpeg_decode(APTX_HD, "aptx_hd", "s32le") {
        assert_eq!(to_s32le(&d.samples), reference);
    }
}

#[test]
fn aptx_with_sample_rate() {
    let reader = AptxReader::try_new_with_rate(mss(APTX), FormatOptions::default(), 44100).unwrap();
    assert_eq!(reader.tracks()[0].time_base.unwrap().denom.get(), 44100);
    let d = decode(Box::new(reader));
    assert_eq!(d.rate, 44100);
}

#[test]
fn aptx_seeks_to_start_only() {
    let mut reader = AptxReader::try_new(mss(APTX), FormatOptions::default()).unwrap();
    let first = reader.next_packet().unwrap().unwrap();

    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1000), track_id: 0 })
            .is_err()
    );

    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0 })
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), 0);
    assert_eq!(reader.next_packet().unwrap().unwrap().data, first.data);
}

#[test]
fn sbc_is_probed() {
    let mut probe = Probe::new();
    probe.register_format::<SbcReader<'_>>();

    for (data, rate) in [(SBC_STEREO, 44100), (SBC_MONO, 16000), (MSBC, 16000)] {
        // With junk before the stream.
        let mut junk = vec![0x55; 37];
        junk.extend_from_slice(data);

        for stream in [data.to_vec(), junk] {
            let reader = probe
                .probe(
                    &Hint::new(),
                    mss(&stream),
                    FormatOptions::default(),
                    MetadataOptions::default(),
                )
                .unwrap();
            assert_eq!(reader.format_info().short_name, "sbc");
            let Some(CodecParameters::Audio(p)) = &reader.tracks()[0].codec_params
            else {
                panic!()
            };
            assert_eq!(p.sample_rate, Some(rate));
        }
    }
}

#[test]
fn sbc_duration_and_seek() {
    let mut reader = SbcReader::try_new(mss(SBC_STEREO), FormatOptions::default()).unwrap();

    // 16 blocks of 8 sub-bands; the bitrate and sampling frequency make a frame 7 ms.
    let dur = reader.tracks()[0].duration.unwrap().get();
    let mut count = 0u64;
    let mut all = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        assert_eq!(p.pts.get() as u64, count * 128);
        assert_eq!(p.dur.get(), 128);
        count += 1;
        all.push(p.data);
    }
    assert_eq!(dur, count * 128);

    let target = 5 * 128 + 17;
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0 })
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), 5 * 128);
    let p = reader.next_packet().unwrap().unwrap();
    assert_eq!(p.pts.get(), 5 * 128);
    assert_eq!(p.data, all[5]);

    assert!(
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(dur as i64), track_id: 0 }
            )
            .is_err()
    );
}

#[test]
fn sbc_resynchronizes_after_junk() {
    // Garbage between the frames is skipped; the frames still decode.
    let clean = decode(sbc_reader(SBC_MONO));

    let mut reader = SbcReader::try_new(mss(SBC_MONO), FormatOptions::default()).unwrap();
    let first = reader.next_packet().unwrap().unwrap();
    let mut damaged = first.data.to_vec();
    damaged.extend_from_slice(&[0x12, 0x34, 0x56]);
    damaged.extend_from_slice(&SBC_MONO[first.data.len()..]);

    let d = decode(sbc_reader(&damaged));
    assert_eq!(d.samples, clean.samples);
}

#[test]
fn sbc_rejects_bad_crc() {
    let mut data = SBC_STEREO.to_vec();
    data[3] ^= 0xff;

    let mut reader = SbcReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    let params = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => panic!(),
    };
    let mut dec = new_decoder(&params);
    let packet = reader.next_packet().unwrap().unwrap();
    assert!(dec.decode(&packet).is_err());
    // The next frame is fine.
    let packet = reader.next_packet().unwrap().unwrap();
    assert!(dec.decode(&packet).is_ok());
}

/// CRC-8 of the bits of the header, as the A2DP specification defines it.
fn crc8_bits(bits: &[bool]) -> u8 {
    let mut crc = 0x0f_u8;
    for &b in bits {
        let top = crc & 0x80 != 0;
        crc <<= 1;
        if top != b {
            crc ^= 0x1d;
        }
    }
    crc
}

fn push_bits(bits: &mut Vec<bool>, value: u32, n: usize) {
    for i in (0..n).rev() {
        bits.push((value >> i) & 1 != 0);
    }
}

/// Builds a frame of random data with a valid header.
fn random_frame(
    rng: &mut XorShift64,
    freq: u8,
    blocks: usize,
    mode: u8,
    snr: bool,
    sb8: bool,
    bitpool: u8,
) -> Vec<u8> {
    let subbands = if sb8 { 8 } else { 4 };
    let channels = if mode == 0 { 1 } else { 2 };
    let b1 =
        freq << 6 | ((blocks / 4 - 1) as u8) << 4 | mode << 2 | u8::from(snr) << 1 | u8::from(sb8);

    let joint = mode == 3;
    let dual = mode == 1;
    let len = 4
        + subbands * channels / 2
        + ((usize::from(dual) + 1) * blocks * usize::from(bitpool) + usize::from(joint) * subbands)
            .div_ceil(8);

    // The CRC covers the header bytes, the joint stereo flags and the scale factors.
    let mut crc_bits = Vec::new();
    push_bits(&mut crc_bits, u32::from(b1), 8);
    push_bits(&mut crc_bits, u32::from(bitpool), 8);

    // The bits after the CRC byte.
    let mut body = Vec::new();

    if joint {
        for sb in 0..subbands {
            // The last flag is reserved.
            let flag = sb + 1 < subbands && rng.below(2) == 1;
            body.push(flag);
            crc_bits.push(flag);
        }
    }

    for _ in 0..channels * subbands {
        let sf = rng.below(11) as u32;
        push_bits(&mut body, sf, 4);
        push_bits(&mut crc_bits, sf, 4);
    }

    let mut data = vec![0x9c, b1, bitpool, crc8_bits(&crc_bits)];
    for chunk in body.chunks(8) {
        let mut byte = 0u8;
        for i in 0..8 {
            byte = byte << 1 | u8::from(chunk.get(i).copied().unwrap_or(rng.below(2) == 1));
        }
        data.push(byte);
    }

    while data.len() < len {
        data.push(rng.next_u64() as u8);
    }
    assert_eq!(data.len(), len);
    data
}

/// Streams of random frames of every channel mode, number of sub-bands, allocation method and
/// sampling frequency, and of random block counts and bitpools, are decoded like FFmpeg does.
#[test]
fn sbc_random_frames_match_ffmpeg() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }

    let mut rng = XorShift64(0x9e37_79b9_7f4a_7c15);
    let mut n = 0;

    for mode in 0..4u8 {
        for sb8 in [false, true] {
            for snr in [false, true] {
                let freq = (n % 4) as u8;
                n += 1;

                let subbands = if sb8 { 8 } else { 4 };
                let max_bitpool = if mode < 2 { 16 * subbands } else { 32 * subbands };

                let mut stream = Vec::new();
                for i in 0..30 {
                    let blocks = 4 * (1 + rng.below(4));
                    let bitpool = match i {
                        0 => max_bitpool,
                        1 => 2,
                        2 => 0,
                        _ => rng.below(max_bitpool + 1),
                    } as u8;
                    stream.extend(random_frame(&mut rng, freq, blocks, mode, snr, sb8, bitpool));
                }

                let d = decode(sbc_reader(&stream));
                let reference = ffmpeg_decode(&stream, "sbc", "s16le").unwrap();
                assert_eq!(
                    to_s16le(&d.samples),
                    reference,
                    "mode {mode}, {subbands} sub-bands, snr {snr}"
                );
            }
        }
    }
}

/// A block with a wrong parity is a synchronization error, which drops the packet.
#[test]
fn aptx_parity_error_drops_packet() {
    let mut data = APTX.to_vec();
    // Flip the parity bit of the last sub-band of the left channel of the first block of the third
    // packet, which is what the blocks are synchronized with.
    data[2048] ^= 0x20;

    let reader = AptxReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    let mut reader: Box<dyn FormatReader> = Box::new(reader);
    let params = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => panic!(),
    };
    let mut dec = new_decoder(&params);

    let mut results = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        results.push(dec.decode(&p).is_ok());
    }

    // The packets before the damaged block decode, the one with it does not.
    assert!(results[..2].iter().all(|&ok| ok));
    assert!(!results[2]);
}

/// Reads the samples of a 16-bit WAVE file written by `dlc3` of `liblc3`.
fn read_wav_s16(data: &[u8]) -> Vec<i16> {
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let n = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &data[pos..pos + 4] == b"data" {
            return data[pos + 8..pos + 8 + n]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
        }
        pos += 8 + n + (n & 1);
    }
    panic!("no data chunk");
}

fn lc3_decoder(reader: &dyn FormatReader) -> Lc3Decoder {
    let Some(CodecParameters::Audio(p)) = &reader.tracks()[0].codec_params
    else {
        panic!("no audio parameters");
    };
    Lc3Decoder::try_new(p, &Default::default()).unwrap()
}

/// Decodes the LC3 files like `dlc3` of `liblc3`, and compares them with its output. The decoder
/// is in single precision, so a sample may be one off in the 16-bit conversion.
#[test]
fn lc3_decodes_like_liblc3() {
    for (data, reference, rate, channels, delay) in
        [(LC3_MONO, LC3_MONO_REF, 16000, 1, 40), (LC3_STEREO, LC3_STEREO_REF, 24000, 2, 60)]
    {
        let mut probe = Probe::new();
        probe.register_format::<Lc3Reader<'_>>();

        let mut reader = probe
            .probe(&Hint::new(), mss(data), FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        assert_eq!(reader.format_info().short_name, "lc3");

        let track = reader.tracks()[0].clone();
        let Some(CodecParameters::Audio(p)) = &track.codec_params
        else {
            panic!()
        };
        assert_eq!(p.sample_rate, Some(rate));
        assert_eq!(p.channels.as_ref().unwrap().count(), channels);
        assert_eq!(track.delay, Some(delay));

        let reference = read_wav_s16(reference);
        let num_frames = track.num_frames.unwrap() as usize;
        assert_eq!(reference.len(), num_frames * channels);
        assert_eq!(track.duration.unwrap().get() as usize, num_frames);

        let mut dec = lc3_decoder(reader.as_ref());
        let mut out = Vec::new();
        let mut frames = 0;
        let mut tmp = Vec::new();

        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(packet.pts.get() as usize, frames);
            frames += packet.dur.get() as usize;
            dec.decode(&packet).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
            out.extend(tmp.iter().map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i32));
        }

        // The delay, the samples, and the padding of the last frame.
        assert_eq!(frames - num_frames - delay as usize, track.padding.unwrap() as usize);

        let out = &out[delay as usize * channels..][..num_frames * channels];
        let worst = out.iter().zip(&reference).map(|(&a, &b)| (a - i32::from(b)).abs()).max();
        assert!(worst.unwrap() <= 1, "worst error {worst:?}");
    }
}

#[test]
fn lc3_seeks_to_a_frame() {
    let mut reader = Lc3Reader::try_new(mss(LC3_STEREO), FormatOptions::default()).unwrap();

    // 5 ms frames of 24 kHz.
    let mut all = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        assert_eq!(p.dur.get(), 120);
        all.push(p.data);
    }

    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(7 * 120 + 5), track_id: 0 },
        )
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), 7 * 120);
    let p = reader.next_packet().unwrap().unwrap();
    assert_eq!(p.pts.get(), 7 * 120);
    assert_eq!(p.data, all[7]);

    let dur = reader.tracks()[0].duration.unwrap().get();
    assert!(
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(dur as i64), track_id: 0 }
            )
            .is_err()
    );
}

#[test]
fn lc3_rejects_bad_headers() {
    // Not a header: wrong signature, bad channel count, invalid sample rate or frame duration.
    for edit in [(0usize, 0x00u8), (8, 0), (8, 9), (4, 0x55), (10, 0x9c)] {
        let mut data = LC3_MONO.to_vec();
        data[edit.0] = edit.1;
        assert!(Lc3Reader::try_new(mss(&data), FormatOptions::default()).is_err(), "{edit:?}");
    }

    // A truncated block is dropped, and so is the end marker.
    let mut reader = Lc3Reader::try_new(mss(&LC3_MONO[..40]), FormatOptions::default()).unwrap();
    assert!(reader.next_packet().unwrap().is_none());
}
