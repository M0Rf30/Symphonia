// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes small streams produced by `speexenc` 1.2.1 and compares them with the output of the
//! floating-point build of libspeex 1.2.1 (compiled without FMA contraction, driven like
//! `speexdec`): the decoded samples must match bit for bit.
//!
//! Each fixture is the 80-byte Speex header followed by the audio packets, each prefixed by its
//! length as a little-endian `u32`. The streams are 26 frames of a synthetic signal that has a
//! silent gap.

use symphonia_codec_speex::SpeexDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

fn split(mut data: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    while !data.is_empty() {
        let n = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        v.push(&data[4..4 + n]);
        data = &data[4 + n..];
    }
    v
}

fn params(extra_data: Option<&[u8]>, sample_rate: Option<u32>) -> AudioCodecParameters {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_SPEEX);
    if let Some(d) = extra_data {
        p.with_extra_data(Box::from(d));
    }
    if let Some(r) = sample_rate {
        p.with_sample_rate(r);
    }
    p
}

fn packet(data: &[u8]) -> PacketRef<'_> {
    PacketRef::new(0, Timestamp::new(0), Duration::new(0), data)
}

/// FNV-1a over the little-endian bytes of the samples.
fn fnv(samples: &[f32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for s in samples {
        for b in s.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Decodes every packet of a fixture to interleaved samples.
fn decode_all(fixture: &[u8]) -> Vec<f32> {
    decode_lossy(fixture, &[])
}

/// Decodes every packet of a fixture, replacing the audio packets with the given indices (0 is
/// the first audio packet) by empty ones, which stand for lost packets.
fn decode_lossy(fixture: &[u8], lost: &[usize]) -> Vec<f32> {
    let pkts = split(fixture);
    let mut dec = SpeexDecoder::try_new(&params(Some(pkts[0]), None), &Default::default()).unwrap();

    let mut out = Vec::new();
    let mut tmp = Vec::new();
    for (i, p) in pkts[1..].iter().enumerate() {
        let p = if lost.contains(&i) { &[][..] } else { p };
        let buf = dec.decode_ref(&packet(p)).unwrap();
        buf.copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

macro_rules! fixture {
    ($name:ident, $file:literal, $samples:expr, $hash:expr) => {
        #[test]
        fn $name() {
            let out = decode_all(include_bytes!(concat!("data/", $file)));
            assert_eq!(out.len(), $samples);
            assert_eq!(fnv(&out), $hash);
        }
    };
}

// Narrow-band, variable bit-rate with DTX: sub-modes 1 to 6 and 8.
fixture!(nb_vbr_dtx, "nb_dtx.pkts", 4160, 0x6679253e92ad099c);
// Narrow-band, sub-mode 7 (double codebook).
fixture!(nb_double_codebook, "nb_q10.pkts", 4160, 0xa1c524ed28a343bf);
// Wide-band, quality 10 (narrow-band sub-mode 7, high-band sub-mode 4).
fixture!(wb_high_quality, "wb_q10.pkts", 8320, 0x7df354db98041e41);
// Wide-band, variable bit-rate (high-band sub-modes 1 and 2).
fixture!(wb_vbr, "wb_vbr.pkts", 8320, 0x9a720d46b9e532d6);
// Ultra-wide-band.
fixture!(uwb, "uwb.pkts", 16640, 0x88e90227d234bca2);
// Narrow-band intensity stereo, two frames per packet.
fixture!(nb_stereo_two_frames, "nb_stereo.pkts", 8320, 0xa03bc238baa8379d);

macro_rules! lossy_fixture {
    ($name:ident, $file:literal, $lost:expr, $samples:expr, $hash:expr) => {
        #[test]
        fn $name() {
            let out = decode_lossy(include_bytes!(concat!("data/", $file)), &$lost);
            assert_eq!(out.len(), $samples);
            assert_eq!(fnv(&out), $hash);
        }
    };
}

// Packet loss concealment. The expected hashes are the output of libspeex 1.2.1 (built as
// above) decoding the same streams with `speex_decode(state, NULL, out)` for the lost packets.
lossy_fixture!(plc_nb, "nb_q10.pkts", [3, 9, 10, 20], 4160, 0xcf694cba1b3f9cc4);
// A burst longer than the ten frames over which the concealment fades out.
lossy_fixture!(
    plc_nb_long_burst,
    "nb_q10.pkts",
    [5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18],
    4160,
    0xa65074b68bbe7af8
);
// Losses inside and around DTX (comfort noise).
lossy_fixture!(plc_nb_dtx, "nb_dtx.pkts", [2, 3, 8, 9, 10, 11, 18], 4160, 0x55895b4a70c778f8);
lossy_fixture!(plc_wb, "wb_q10.pkts", [4, 5, 12, 13, 14, 22], 8320, 0xb0742f947cf94c08);
lossy_fixture!(plc_wb_vbr, "wb_vbr.pkts", [3, 4, 5, 6, 15], 8320, 0x0564ef5c4b84cc78);
lossy_fixture!(plc_uwb, "uwb.pkts", [6, 7, 8, 16], 16640, 0xf9167a53be4e7732);

#[test]
fn without_header_the_mode_follows_the_sample_rate() {
    // As in FLV: no header, one frame per packet, 16 kHz is wide-band.
    let fixture = include_bytes!("data/wb_q10.pkts");
    let pkts = split(fixture);
    let mut dec = SpeexDecoder::try_new(&params(None, Some(16000)), &Default::default()).unwrap();

    let mut out = Vec::new();
    let mut tmp = Vec::new();
    for p in &pkts[1..] {
        dec.decode_ref(&packet(p)).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    assert_eq!(fnv(&out), 0x7df354db98041e41);

    // Without a header or a sample rate there is nothing to go by.
    assert!(SpeexDecoder::try_new(&params(None, None), &Default::default()).is_err());
}

#[test]
fn rejects_invalid_headers() {
    let fixture = include_bytes!("data/nb_q10.pkts");
    let header = split(fixture)[0].to_vec();
    let opts = AudioDecoderOptions::default();

    assert!(SpeexDecoder::try_new(&params(Some(&header), None), &opts).is_ok());

    // Bad signature.
    let mut h = header.clone();
    h[0] = b'X';
    assert!(SpeexDecoder::try_new(&params(Some(&h), None), &opts).is_err());

    // Unknown mode.
    let mut h = header.clone();
    h[40] = 3;
    assert!(SpeexDecoder::try_new(&params(Some(&h), None), &opts).is_err());

    // Unsupported bit-stream version.
    let mut h = header.clone();
    h[44] = 5;
    assert!(SpeexDecoder::try_new(&params(Some(&h), None), &opts).is_err());

    // Unsupported Speex version.
    let mut h = header.clone();
    h[28] = 2;
    assert!(SpeexDecoder::try_new(&params(Some(&h), None), &opts).is_err());

    // Absurd number of frames per packet.
    let mut h = header.clone();
    h[64..68].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(SpeexDecoder::try_new(&params(Some(&h), None), &opts).is_err());

    // Another codec.
    let mut p = params(Some(&header), None);
    p.codec = symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
    assert!(SpeexDecoder::try_new(&p, &opts).is_err());
}

#[test]
fn truncated_and_corrupt_packets_do_not_panic() {
    let fixture = include_bytes!("data/wb_q10.pkts");
    let pkts = split(fixture);
    let mut dec = SpeexDecoder::try_new(&params(Some(pkts[0]), None), &Default::default()).unwrap();

    for p in &pkts[1..] {
        // A truncated frame is an error or is dropped, but never yields partial data. (An
        // empty packet is a lost one, see `plc_*`.)
        for len in [1, 2, p.len() / 2] {
            if let Ok(buf) = dec.decode_ref(&packet(&p[..len])) {
                assert_eq!(buf.frames(), 0);
            }
        }

        // Flipped bits.
        let mut d = p.to_vec();
        for i in 0..d.len() * 8 {
            d[i / 8] ^= 1 << (i % 8);
            let _ = dec.decode_ref(&packet(&d));
            d[i / 8] ^= 1 << (i % 8);
        }

        // The undamaged packet still decodes.
        assert_eq!(dec.decode_ref(&packet(p)).unwrap().frames(), 320);
    }
}

#[test]
fn gapless_trim_and_reset() {
    let fixture = include_bytes!("data/nb_q10.pkts");
    let pkts = split(fixture);

    let mut opts = AudioDecoderOptions::default();
    opts.gapless = true;
    let mut dec = SpeexDecoder::try_new(&params(Some(pkts[0]), None), &opts).unwrap();

    let mut pr = packet(pkts[1]);
    pr.trim_start = Duration::new(40);
    pr.trim_end = Duration::new(10);
    assert_eq!(dec.decode_ref(&pr).unwrap().frames(), 110);

    // Resetting restores the initial state: the stream decodes identically a second time.
    let first: Vec<f32> = {
        let mut dec = SpeexDecoder::try_new(&params(Some(pkts[0]), None), &opts).unwrap();
        let mut v = Vec::new();
        dec.decode_ref(&packet(pkts[1])).unwrap().copy_to_vec_interleaved::<f32>(&mut v);
        v
    };
    for p in &pkts[1..6] {
        dec.decode_ref(&packet(p)).unwrap();
    }
    dec.reset();
    let mut again = Vec::new();
    dec.decode_ref(&packet(pkts[1])).unwrap().copy_to_vec_interleaved::<f32>(&mut again);
    assert_eq!(first, again);
}

#[test]
fn short_last_packet_keeps_its_valid_samples() {
    // The header announces two frames per packet, but the packet holds one: like the last packet
    // of a stream written with `--nframes 2` and an odd number of frames.
    let fixture = include_bytes!("data/nb_q10.pkts");
    let pkts = split(fixture);
    let mut header = pkts[0].to_vec();
    header[64..68].copy_from_slice(&2u32.to_le_bytes());

    let mut opts = AudioDecoderOptions::default();
    opts.gapless = true;
    let mut dec = SpeexDecoder::try_new(&params(Some(&header), None), &opts).unwrap();

    // The demuxer computes the padding for the nominal two frames (320 samples) of which 160
    // are valid: all of the frame that was decoded must be kept.
    let mut pr = packet(pkts[1]);
    pr.trim_end = Duration::new(160);
    assert_eq!(dec.decode_ref(&pr).unwrap().frames(), 160);

    // Padding beyond the missing frame still applies.
    pr.trim_end = Duration::new(170);
    assert_eq!(dec.decode_ref(&pr).unwrap().frames(), 150);
}
