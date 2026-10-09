// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests using real-world files written by ffmpeg.
//!
//! The files are not part of the repository. The tests look for them in the directory named by the
//! `RMPD_SAMPLES` environment variable (the `mkv` sub-directory of the sample set), and are
//! skipped if it is not set, or the file does not exist.

use std::fs::File;

use symphonia_codec_wavpack::WavPackDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::units::Time;
use symphonia_format_mkv::MkvReader;

fn open(name: &str) -> Option<MkvReader<'static>> {
    let dir = std::env::var_os("RMPD_SAMPLES")?;
    let path = std::path::Path::new(&dir).join("mkv").join(name);

    let file = File::open(&path).ok().or_else(|| {
        eprintln!("skipping: {} does not exist", path.display());
        None
    })?;

    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    Some(MkvReader::try_new(mss, FormatOptions::default()).expect("sample should open"))
}

/// Read all remaining packets. The end of the stream must be signalled with `Ok(None)`.
fn read_all(reader: &mut MkvReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("reading should not fail") {
        packets.push(packet);
    }
    packets
}

fn seek_secs(
    reader: &mut MkvReader<'_>,
    secs: f64,
) -> Result<SeekedTo, symphonia_core::errors::Error> {
    let time = Time::try_from_secs_f64(secs).unwrap();
    reader.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: None })
}

/// WavPack blocks in Matroska have no 32-byte block header. They must be decoded exactly to the
/// same samples as the PCM the file was created from, and never abort the process.
#[test]
fn wavpack_in_matroska_decodes_losslessly() {
    let (Some(mut wavpack), Some(mut pcm)) = (open("mka_wavpack.mka"), open("mka_pcm_tags.mka"))
    else {
        return;
    };

    let params = match wavpack.tracks()[0].codec_params.clone() {
        Some(CodecParameters::Audio(params)) => params,
        _ => panic!("expected audio codec parameters"),
    };

    let mut decoder = WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    // Compare 2s of audio.
    let mut expected: Vec<i16> = Vec::new();
    while expected.len() < 2 * 44100 * 2 {
        let packet = pcm.next_packet().unwrap().unwrap();
        expected.extend(packet.data.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])));
    }

    let mut decoded: Vec<i16> = Vec::new();
    let mut block: Vec<i16> = Vec::new();
    while decoded.len() < expected.len() {
        let packet = wavpack.next_packet().unwrap().unwrap();
        let buf = decoder.decode(&packet).expect("the packet should decode");
        buf.copy_to_vec_interleaved(&mut block);
        decoded.extend_from_slice(&block);
    }

    let n = expected.len();
    assert!(decoded[..n] == expected[..n], "decoded samples differ from the source");

    // Seeking, and decoding after a seek, is also lossless.
    let seeked = seek_secs(&mut wavpack, 12.5).unwrap();
    assert_eq!(seeked.actual_ts.get() % 4096, 0);
    decoder.reset();

    let packet = wavpack.next_packet().unwrap().unwrap();
    let buf = decoder.decode(&packet).expect("the packet should decode");
    let mut decoded: Vec<i16> = Vec::new();
    buf.copy_to_vec_interleaved(&mut decoded);

    let seeked = seek_secs(&mut pcm, 12.5).unwrap();
    assert_eq!(seeked.actual_ts, packet.pts);
    let packet = pcm.next_packet().unwrap().unwrap();
    let expected: Vec<i16> =
        packet.data.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect();

    assert!(decoded == expected, "decoded samples after a seek differ from the source");
}

/// All of the stream of a decoder is decoded without errors.
#[test]
fn wavpack_in_matroska_decodes_to_the_end() {
    let Some(mut reader) = open("mka_wavpack.mka")
    else {
        return;
    };

    let params = match reader.tracks()[0].codec_params.clone() {
        Some(CodecParameters::Audio(params)) => params,
        _ => panic!("expected audio codec parameters"),
    };

    let mut decoder = WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut frames = 0;
    for packet in read_all(&mut reader) {
        frames += decoder.decode(&packet).expect("the packet should decode").frames();
    }

    // 30s of audio (the last block is padded in the muxer's timestamps only).
    assert_eq!(frames, 30 * 44100);
}

/// The number of valid frames in an Opus stream is the number of frames that were encoded.
#[test]
fn opus_discard_padding_is_trimmed() {
    for name in ["mka_opus_tags.mka", "mka_unknown_duration_live.mka"] {
        let Some(mut reader) = open(name)
        else {
            continue;
        };

        assert_eq!(reader.tracks()[0].delay, Some(312));

        let packets = read_all(&mut reader);
        let valid: u64 = packets.iter().map(|packet| packet.dur.get()).sum();

        // Exactly 30s.
        assert_eq!(valid, 30 * 48000, "{name}");
        assert_eq!(packets.first().map(|p| (p.pts.get(), p.trim_start.get())), Some((-312, 312)));
        assert_eq!(packets.last().map(|p| p.trim_end.get()), Some(648));

        // The packets are contiguous.
        for pair in packets.windows(2) {
            assert_eq!(pair[0].pts.get() + pair[0].block_dur().get() as i64, pair[1].pts.get());
        }
    }
}

/// Seek from the end of the stream back into it, for various codecs.
#[test]
fn seek_after_end_of_stream() {
    let names = [
        "mka_flac_tags.mka",
        "mka_alac.mka",
        "mka_attachment_cover.mka",
        "mka_vorbis_tags.mka",
        "mka_mp3_vbr.mka",
        "mka_aac_tags.mka",
        "mka_opus_tags.mka",
        "mka_pcm_tags.mka",
    ];

    for name in names {
        let Some(mut reader) = open(name)
        else {
            continue;
        };

        let all = read_all(&mut reader);
        assert!(reader.next_packet().unwrap().is_none(), "{name}");

        for secs in [10.0, 0.0, 29.0, 5.0] {
            let seeked = seek_secs(&mut reader, secs).unwrap_or_else(|e| panic!("{name}: {e}"));

            // An accurate seek never lands after the requested position, and lands close to it.
            let tb = reader.tracks()[0].time_base.unwrap();
            assert!(seeked.actual_ts <= seeked.required_ts, "{name}");
            let behind = (seeked.required_ts.get() - seeked.actual_ts.get()) as f64
                * f64::from(tb.numer.get())
                / f64::from(tb.denom.get());
            assert!(behind < 0.5, "{name}: {secs}s");

            // The packets read after the seek are the packets of the stream.
            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(packet.pts, seeked.actual_ts, "{name}");
            let first =
                all.iter().position(|p| p.data == packet.data).expect("packet should exist");
            // The timestamps of packets in a sequential read may be refined from the
            // millisecond-precision timestamps of the blocks, by up to a millisecond.
            let diff = (all[first].pts.get() - packet.pts.get()).unsigned_abs() as f64
                * f64::from(tb.numer.get())
                / f64::from(tb.denom.get());
            assert!(diff <= 0.001, "{name}");
        }
    }
}

#[test]
fn chapters_without_an_edition_uid() {
    let Some(reader) = open("mka_chapters.mka")
    else {
        return;
    };

    assert!(reader.chapters().is_some());
}

#[test]
fn cover_attachment_is_a_visual() {
    let Some(mut reader) = open("mka_attachment_cover.mka")
    else {
        return;
    };

    assert_eq!(reader.attachments().len(), 1);

    let metadata = reader.metadata();
    let revision = metadata.current().unwrap();
    assert_eq!(revision.media.visuals.len(), 1);
    assert!(revision.media.visuals[0].dimensions.is_some());
}
