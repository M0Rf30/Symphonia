// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "mp3pro")]
#![allow(clippy::unwrap_used)]

//! Whether the mp3PRO decoder outputs the SBR output (twice the core sample rate) or the core
//! audio depends on the stream: only streams that carry SBR data are mp3PRO streams. Plain
//! MPEG-2/2.5 Layer III streams below 32 kHz decode exactly like [`MpaDecoder`].
//!
//! The tests that use real mp3PRO files or other MP3 files of the corpus named by the
//! `RMPD_SAMPLES` environment variable are skipped if the files are not present.

use std::fs::File;
use std::io::Cursor;

use symphonia_bundle_mp3::{Mp3ProDecoder, MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_testutil::sample_path;

const MONO_22K: &[u8] = include_bytes!("fixtures/mp3_mpeg2_mono_22k.mp3");
const JOINT_22K: &[u8] = include_bytes!("fixtures/mp3_mpeg2_joint_22k.mp3");
const STEREO_44K: &[u8] = include_bytes!("fixtures/mp3_cbr128_st_44k_notag.mp3");

/// The audio decoded from a sequence of packets: the output specification of every decoded
/// packet, the samples, and the number of resets requested.
#[derive(Debug, Default, PartialEq)]
struct Decoded {
    specs: Vec<(u32, usize)>,
    samples: Vec<f32>,
    resets: usize,
}

fn open(mss: MediaSourceStream<'static>) -> (MpaReader<'static>, AudioCodecParameters) {
    let reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();
    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone();
    (reader, params)
}

fn packets(mss: MediaSourceStream<'static>, max: usize) -> (Vec<Packet>, AudioCodecParameters) {
    let (mut reader, params) = open(mss);
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        if packets.len() >= max {
            break;
        }
        packets.push(packet);
    }

    (packets, params)
}

fn packets_of(stream: &[u8]) -> (Vec<Packet>, AudioCodecParameters) {
    packets(
        MediaSourceStream::new(Box::new(Cursor::new(stream.to_vec())), Default::default()),
        usize::MAX,
    )
}

fn sample_packets(name: &str, max: usize) -> Option<(Vec<Packet>, AudioCodecParameters)> {
    let path = sample_path(name)?;
    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    Some(packets(mss, max))
}

/// Decodes the packets. A packet that requests a reset is decoded again.
fn decode(decoder: &mut dyn AudioDecoder, packets: &[Packet]) -> Decoded {
    let mut out = Decoded::default();

    for packet in packets {
        let mut result = decoder.decode(packet).map(|_| ());

        if let Err(Error::ResetRequired) = result {
            out.resets += 1;
            assert_eq!(decoder.last_decoded().frames(), 0);
            result = decoder.decode(packet).map(|_| ());
        }

        result.unwrap();

        let decoded = decoder.last_decoded();
        out.specs.push((decoded.spec().rate(), decoded.spec().channels().count()));

        let mut buf = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        out.samples.extend_from_slice(&buf);
    }

    out
}

fn mpa(params: &AudioCodecParameters, gapless: bool) -> MpaDecoder {
    MpaDecoder::try_new(params, &AudioDecoderOptions::default().gapless(gapless)).unwrap()
}

fn mp3pro(params: &AudioCodecParameters, gapless: bool) -> Mp3ProDecoder {
    Mp3ProDecoder::try_new(params, &AudioDecoderOptions::default().gapless(gapless)).unwrap()
}

#[test]
fn verify_plain_streams_decode_like_the_iso_decoder() {
    for (name, stream, rate, channels) in
        [("mono", MONO_22K, 22050, 1), ("joint stereo", JOINT_22K, 22050, 2)]
    {
        let (packets, params) = packets_of(stream);

        for gapless in [false, true] {
            let plain = decode(&mut mpa(&params, gapless), &packets);
            let pro = decode(&mut mp3pro(&params, gapless), &packets);

            assert_eq!(pro.resets, 0, "{name}");
            assert!(pro.specs.iter().all(|&spec| spec == (rate, channels)), "{name}");
            assert!(!pro.samples.is_empty(), "{name}");
            assert_eq!(pro, plain, "{name}, gapless={gapless}");
        }
    }
}

#[test]
fn verify_cores_that_never_carry_sbr_decode_like_the_iso_decoder() {
    let (packets, params) = packets_of(STEREO_44K);
    let plain = decode(&mut mpa(&params, true), &packets);
    let pro = decode(&mut mp3pro(&params, true), &packets);

    assert_eq!(pro.specs[0], (44100, 2));
    assert_eq!(pro, plain);
}

#[test]
fn verify_seeking_in_a_plain_stream_keeps_the_core_output() {
    let (packets, params) = packets_of(JOINT_22K);
    let mut decoder = mp3pro(&params, false);
    decode(&mut decoder, &packets);

    decoder.reset();
    let again = decode(&mut decoder, &packets[2..]);
    assert_eq!(again.resets, 0);
    assert!(again.specs.iter().all(|&spec| spec == (22050, 2)));
}

/// Every plain MPEG-2/2.5 stream of the corpus decodes like the ISO decoder, packet by packet
/// (including the packets that fail to decode). None of them carries SBR data.
#[test]
fn verify_corpus_mp3_files_decode_like_the_iso_decoder() {
    let Some(dir) = sample_path("mp3")
    else {
        return;
    };

    let mut checked = 0;

    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();

        if path.extension().is_none_or(|ext| ext != "mp3") {
            continue;
        }

        let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
        let Ok(mut reader) = MpaReader::try_new(mss, FormatOptions::default())
        else {
            continue;
        };

        let Some(params) = reader.tracks()[0].codec_params.as_ref().and_then(|p| p.audio())
        else {
            continue;
        };
        let params = params.clone();

        let (mut plain, mut pro) = (mpa(&params, true), mp3pro(&params, true));
        let (mut buf_a, mut buf_b) = (Vec::<f32>::new(), Vec::<f32>::new());
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        while let Ok(Some(packet)) = reader.next_packet() {
            let a = plain.decode(&packet).map(|decoded| {
                decoded.copy_to_vec_interleaved(&mut buf_a);
                decoded.spec().clone()
            });
            let b = pro.decode(&packet).map(|decoded| {
                decoded.copy_to_vec_interleaved(&mut buf_b);
                decoded.spec().clone()
            });

            assert_eq!(a.is_ok(), b.is_ok(), "{name}: {a:?} {b:?}");

            if let (Ok(a), Ok(b)) = (a, b) {
                assert_eq!(a, b, "{name}");
                assert_eq!(buf_a, buf_b, "{name}");
            }
        }

        checked += 1;
    }

    assert!(checked > 0);
}

/// A plain stream that is followed by a stream with SBR data (with the same core specification):
/// the output specification changes with a reset at the first packet with SBR data, and that
/// packet is decoded as if it started the stream.
#[test]
fn verify_sbr_data_after_a_plain_stream_switches_the_output() {
    let Some((sbr, _)) = sample_packets("mp3pro/daft.mp3", 40)
    else {
        return;
    };

    let (plain, params) = packets_of(JOINT_22K);
    let alone = decode(&mut mp3pro(&params, false), &sbr);
    assert_eq!(alone.resets, 0);
    assert!(alone.specs.iter().all(|&spec| spec == (44100, 2)));

    let all: Vec<Packet> = plain.iter().chain(sbr.iter()).cloned().collect();
    let pro = decode(&mut mp3pro(&params, false), &all);

    assert_eq!(pro.resets, 1);
    assert!(pro.specs[..plain.len()].iter().all(|&spec| spec == (22050, 2)));
    assert!(pro.specs[plain.len()..].iter().all(|&spec| spec == (44100, 2)));

    // The plain part decodes like the plain stream on its own, and the stream with SBR data
    // decodes like it does on its own.
    let plain_alone = decode(&mut mp3pro(&params, false), &plain);
    let split = plain_alone.samples.len();
    assert_eq!(pro.samples[..split], plain_alone.samples[..]);
    assert_eq!(pro.samples[split..], alone.samples[..]);
}

/// mp3PRO streams are mp3PRO streams from the first packet.
#[test]
fn verify_sbr_streams_output_twice_the_core_rate_from_the_first_packet() {
    for name in ["daft.mp3", "scooter.mp3", "solaar.mp3"] {
        let Some((sbr, params)) = sample_packets(&format!("mp3pro/{name}"), 20)
        else {
            continue;
        };

        assert_eq!(params.sample_rate, Some(22050));

        let pro = decode(&mut mp3pro(&params, false), &sbr);
        assert_eq!(pro.resets, 0, "{name}");
        assert!(pro.specs.iter().all(|&spec| spec == (44100, 2)), "{name}");
    }
}

/// The core of an mp3PRO stream with SBR (MPEG-2 at 22.05 kHz) and the core of a plain MP3 stream
/// (MPEG-1 at 44.1 kHz) output the same specification, 44.1 kHz stereo, but the second stream
/// cannot continue the decoder state of the first.
#[test]
fn verify_core_change_with_same_output_spec_requires_reset() {
    let Some((sbr, params)) = sample_packets("mp3pro/daft.mp3", 40)
    else {
        return;
    };

    let (plain, plain_params) = packets_of(STEREO_44K);
    let all: Vec<Packet> = sbr.iter().chain(plain.iter()).cloned().collect();

    let sbr_alone = decode(&mut mp3pro(&params, false), &sbr);
    let pro = decode(&mut mp3pro(&params, false), &all);

    assert_eq!(pro.resets, 1);
    assert!(pro.specs.iter().all(|&spec| spec == (44100, 2)));

    // Both streams are decoded as if they were on their own.
    let split = sbr_alone.samples.len();
    assert_eq!(pro.samples[..split], sbr_alone.samples[..]);

    let plain_alone = decode(&mut mp3pro(&plain_params, false), &plain);
    assert_eq!(pro.samples[split..], plain_alone.samples[..]);
}
