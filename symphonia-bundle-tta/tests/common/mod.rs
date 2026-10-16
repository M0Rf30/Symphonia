// Symphonia TTA integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use symphonia_bundle_tta::{TtaDecoder, TtaReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_testutil::Fnv;

/// The small fixtures of `tests/data`, all made by `ffmpeg -c:a tta`.
pub const FIXTURES: &[&str] = &[
    "mono_s16_8k",
    "stereo_s16_8k",
    "stereo_u8_8k",
    "stereo_s24_8k",
    "stereo_s16_11k",
    "stereo_s16_4k_exact",
    "mono_s16_8k_tiny",
    "stereo_s16_8k_silence",
    "ch3_s16_8k",
    "ch4_s16_8k",
    "ch5_s16_8k",
    "ch6_s16_8k",
    "ch7_s16_8k",
    "ch8_s16_8k",
];

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(format!("{name}.tta"))
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

pub fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

pub fn open(data: Vec<u8>) -> TtaReader<'static> {
    TtaReader::try_new(mss(data), FormatOptions::default()).expect("open")
}

pub fn audio_params(reader: &dyn FormatReader) -> AudioCodecParameters {
    match reader.tracks()[0].codec_params.as_ref().expect("codec params") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("expected audio"),
    }
}

pub fn decoder(reader: &dyn FormatReader, verify: bool) -> TtaDecoder {
    let opts = AudioDecoderOptions::default().verify(verify);
    TtaDecoder::try_new(&audio_params(reader), &opts).expect("decoder")
}

/// The decoded stream, as the interleaved PCM FFmpeg writes for the sample format of the stream
/// (`u8`, `s16le`, or `s32le` with the samples of a 24-bit stream shifted to the top 24 bits).
pub struct Decoded {
    pub bytes_per_sample: usize,
    pub channels: usize,
    pub frames: u64,
    pub pcm: Vec<u8>,
    /// The number of packets the decoder rejected.
    pub errors: usize,
}

impl Decoded {
    pub fn fnv(&self) -> u64 {
        let mut fnv = Fnv::new();
        fnv.write(&self.pcm);
        fnv.finish()
    }
}

/// Appends the `S32` buffer left-justified in 32 bits, as the decoder wrote it, to `pcm` in the
/// byte format FFmpeg uses for streams of `bytes` bytes per sample.
fn push_pcm(pcm: &mut Vec<u8>, buf: &GenericAudioBufferRef<'_>, bytes: usize) -> (usize, usize) {
    let GenericAudioBufferRef::S32(b) = buf
    else {
        panic!("expected an S32 buffer");
    };

    let channels = b.spec().channels().count();
    let frames = b.frames();

    for i in 0..frames {
        for c in 0..channels {
            let v = b.plane(c).expect("plane")[i];
            match bytes {
                1 => pcm.push(((v >> 24) as u8) ^ 0x80),
                2 => pcm.extend_from_slice(&((v >> 16) as i16).to_le_bytes()),
                _ => pcm.extend_from_slice(&v.to_le_bytes()),
            }
        }
    }

    (channels, frames)
}

/// Decodes the whole stream. A packet the decoder rejects is skipped, as ffmpeg does. The read
/// ends on the first error of the reader.
pub fn decode(data: Vec<u8>, verify: bool) -> Decoded {
    let mut reader = open(data);
    decode_reader(&mut reader, verify)
}

pub fn decode_reader(reader: &mut TtaReader<'_>, verify: bool) -> Decoded {
    let params = audio_params(reader);
    let bytes_per_sample = (params.bits_per_sample.expect("bits per sample") as usize).div_ceil(8);
    let mut dec = decoder(reader, verify);

    let mut out =
        Decoded { bytes_per_sample, channels: 0, frames: 0, pcm: Vec::new(), errors: 0 };

    while let Ok(Some(packet)) = reader.next_packet() {
        match dec.decode(&packet) {
            Ok(buf) => {
                let (channels, frames) = push_pcm(&mut out.pcm, &buf, bytes_per_sample);
                out.channels = channels;
                out.frames += frames as u64;
            }
            Err(Error::DecodeError(_)) => out.errors += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    out
}

/// What FFmpeg decodes `data` to, in the format of `decoded`, or `None` when FFmpeg is not
/// installed. The exit status is ignored: damaged streams are decoded as far as possible.
pub fn ffmpeg_pcm(data: &[u8], bytes_per_sample: usize) -> Option<Vec<u8>> {
    let fmt = match bytes_per_sample {
        1 => "u8",
        2 => "s16le",
        _ => "s32le",
    };

    // A unique name, since the tests run in parallel.
    let mut fnv = Fnv::new();
    fnv.write(data);
    let name = format!("symphonia-tta-{}-{:016x}.tta", std::process::id(), fnv.finish());
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, data).expect("write temp");

    let out = Command::new("ffmpeg")
        .args(["-v", "quiet", "-nostdin", "-i"])
        .arg(&path)
        .args(["-f", fmt, "-"])
        .stdin(Stdio::null())
        .output();

    let _ = std::fs::remove_file(&path);

    out.ok().map(|o| o.stdout)
}

/// The little-endian `u32` of `data` at `at`.
pub fn le32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// The number of frames of the stream, from the header.
pub fn frame_count(data: &[u8]) -> usize {
    let frame_len = 256 * u64::from(le32(data, 10)) / 245;
    u64::from(le32(data, 14)).div_ceil(frame_len) as usize
}

/// The position of the first frame: after the header, the seek table, and its CRC.
pub fn first_frame_pos(data: &[u8]) -> usize {
    22 + 4 * frame_count(data) + 4
}
