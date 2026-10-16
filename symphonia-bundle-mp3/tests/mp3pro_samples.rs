// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "mp3pro")]
#![allow(clippy::unwrap_used)]

//! Real mp3PRO files. The files are located in the `mp3pro` directory of the corpus named by the
//! `RMPD_SAMPLES` environment variable. Tests are skipped if the files are not present.
//!
//! The pinned values are the FNV-1a hash of the 16-bit rounded output of [`Mp3ProDecoder`]. The
//! output was compared with the output of the Thomson mp3PRO decoder (`libmp3PRO.so` 1.0.1) over
//! each whole file: the output of the two decoders has the same length and alignment, and differs
//! by at most 0.25 of the least significant bit of 16-bit audio.

use std::fs::File;

use symphonia_bundle_mp3::{Mp3ProDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_testutil::{Fnv, sample_path};

/// The delay of the SBR output, in samples per channel at the output rate.
const SBR_DELAY: usize = 962;

/// The result of the decode of a file.
struct Decoded {
    /// The sample rate and channel count of the output.
    rate: u32,
    channels: usize,
    /// The number of packets.
    packets: usize,
    /// The number of samples per channel.
    frames: usize,
    /// The hash of the output samples.
    hash: u64,
    /// The hash of the output samples after the first `skip` samples per channel.
    hash_skipped: u64,
}

fn decode(name: &str, gapless: bool, skip: usize) -> Option<Decoded> {
    let path = sample_path(format!("mp3pro/{name}"))?;
    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();
    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone();
    let mut decoder =
        Mp3ProDecoder::try_new(&params, &AudioDecoderOptions::default().gapless(gapless)).unwrap();

    let (mut hash, mut hash_skipped) = (Fnv::new(), Fnv::new());
    let (mut rate, mut channels, mut packets, mut frames) = (0, 0, 0, 0);
    let mut buf = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        let decoded = decoder.decode(&packet).unwrap();
        (rate, channels) = (decoded.spec().rate(), decoded.spec().channels().count());
        packets += 1;

        decoded.copy_to_vec_interleaved::<f32>(&mut buf);

        for (i, s) in buf.iter().enumerate() {
            let bytes = ((s * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes();
            hash.write(&bytes);

            if frames + i / channels >= skip {
                hash_skipped.write(&bytes);
            }
        }

        frames += decoded.frames();
    }

    Some(Decoded {
        rate,
        channels,
        packets,
        frames,
        hash: hash.finish(),
        hash_skipped: hash_skipped.finish(),
    })
}

/// The sample, its core rate and channel count, and the hash of the output.
const SAMPLES: &[(&str, u32, usize, u64)] = &[
    ("daft.mp3", 22050, 2, 0xbe7c68f5ccab9d85),
    ("scooter.mp3", 22050, 2, 0x57e5d81d93e174a5),
    ("solaar.mp3", 22050, 2, 0xa8fc0f67a7e1f215),
];

#[test]
fn verify_output_matches_the_thomson_decoder() {
    for &(name, core_rate, channels, hash) in SAMPLES {
        let Some(full) = decode(name, false, SBR_DELAY)
        else {
            continue;
        };

        // The SBR output has twice the core rate, and every packet decodes to a whole frame.
        assert_eq!((full.rate, full.channels), (2 * core_rate, channels), "{name}");
        assert_eq!(full.frames, full.packets * 1152, "{name}");
        assert_eq!(full.hash, hash, "{name}: {:#x}", full.hash);
    }
}

#[test]
fn verify_gapless_removes_the_sbr_delay() {
    for &(name, ..) in SAMPLES {
        // The streams have no encoder delay or padding, so only the delay of the SBR output is
        // removed.
        let Some(full) = decode(name, false, SBR_DELAY)
        else {
            continue;
        };
        let gapless = decode(name, true, 0).unwrap();

        assert_eq!(gapless.frames, full.frames - SBR_DELAY, "{name}");
        assert_eq!(gapless.hash, full.hash_skipped, "{name}");
    }
}
