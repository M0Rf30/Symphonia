// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the integration tests.

#![allow(dead_code, clippy::unwrap_used)]

use std::io::Cursor;
use std::process::{Command, Stdio};

use symphonia_codec_adpcm::AdpcmDecoder;
use symphonia_codec_pcm::PcmDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::io::MediaSourceStream;
use symphonia_testutil::Fnv;

pub fn mss(data: &[u8]) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default())
}

/// What a stream decodes to.
pub struct Decoded {
    pub rate: u32,
    pub channels: usize,
    pub packets: usize,
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
}

/// Decodes every packet of `reader` with the ADPCM decoder. Trimming is applied only if `gapless`
/// is set.
pub fn decode(reader: &mut dyn FormatReader, gapless: bool) -> Decoded {
    let track = reader.tracks()[0].clone();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };

    let mut opts = AudioDecoderOptions::default();
    opts.gapless = gapless;
    let mut dec: Box<dyn AudioDecoder> = match AdpcmDecoder::try_new(params, &opts) {
        Ok(dec) => Box::new(dec),
        Err(_) => Box::new(PcmDecoder::try_new(params, &opts).unwrap()),
    };

    let mut out = Decoded { rate: 0, channels: 0, packets: 0, samples: Vec::new() };

    while let Some(packet) = reader.next_packet().unwrap() {
        out.packets += 1;
        let buf = dec.decode(&packet).unwrap();
        out.rate = buf.spec().rate();
        out.channels = buf.spec().channels().count();
        let mut tmp = Vec::new();
        buf.copy_to_vec_interleaved::<i16>(&mut tmp);
        out.samples.extend_from_slice(&tmp);
    }

    out
}

/// Decodes `data` with FFmpeg. `format` is the demuxer, and `pcm` the raw output format. Returns
/// None if FFmpeg is not available.
pub fn ffmpeg_decode(data: &[u8], format: &str, pcm: &str) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("symphonia-game-{}", std::process::id()));
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

pub fn to_s16le(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// The FNV-1a hash of the 16-bit little-endian bytes of `samples`.
pub fn fnv(samples: &[i16]) -> u64 {
    let mut h = Fnv::new();
    h.write(&to_s16le(samples));
    h.finish()
}

/// Checks a hash, or prints it if `GOLDEN_PRINT` is set, to regenerate the expected values.
pub fn check_hash(name: &str, samples: &[i16], expected: u64) {
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        println!("{name}: {:#018x}", fnv(samples));
    }
    else {
        assert_eq!(fnv(samples), expected, "{name}");
    }
}
