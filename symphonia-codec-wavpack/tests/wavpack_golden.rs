// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Golden hashes of the full decode of every WavPack fixture and (when `RMPD_SAMPLES` is set)
//! every sample of the corpus, so that decoder and reader optimisations are provably
//! bit-identical, plus seek tests that compare against the sequential decode.
//!
//! Run with `GOLDEN_PRINT=1 cargo test -p symphonia-codec-wavpack --test wavpack_golden -- --nocapture`
//! to print the table for the checked-in `GOLDEN` constant.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader, with_sibling_correction};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

fn mss(path: &Path) -> MediaSourceStream<'static> {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    MediaSourceStream::new(Box::new(file), Default::default())
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn feed(h: &mut Fnv, buf: &GenericAudioBufferRef<'_>) {
    let n = buf.frames();
    let nch = buf.spec().channels().count();
    macro_rules! planes {
        ($b:expr, $conv:expr) => {
            for i in 0..n {
                for ch in 0..nch {
                    h.write(&$conv($b.plane(ch).unwrap()[i]));
                }
            }
        };
    }
    match buf {
        GenericAudioBufferRef::S8(b) => planes!(b, |v: i8| v.to_le_bytes()),
        GenericAudioBufferRef::S16(b) => planes!(b, |v: i16| v.to_le_bytes()),
        GenericAudioBufferRef::S24(b) => {
            planes!(b, |v: symphonia_core::audio::sample::i24| v.to_le_bytes())
        }
        GenericAudioBufferRef::S32(b) => planes!(b, |v: i32| v.to_le_bytes()),
        GenericAudioBufferRef::F32(b) => planes!(b, |v: f32| v.to_le_bytes()),
        _ => panic!("unhandled sample format"),
    }
}

fn make_decoder(reader: &dyn FormatReader) -> WavPackDecoder {
    let params: AudioCodecParameters = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("no audio codec params"),
    };
    WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder")
}

/// Hash of every decoded packet (errors are part of the hash) of the remaining stream.
fn hash_decode(reader: &mut dyn FormatReader) -> (u64, u64) {
    let mut decoder = make_decoder(reader);
    let mut h = Fnv::new();
    let mut frames = 0u64;
    while let Ok(Some(packet)) = reader.next_packet() {
        match decoder.decode_ref(&packet.as_packet_ref()) {
            Ok(buf) => {
                frames += buf.frames() as u64;
                feed(&mut h, &buf);
            }
            Err(_) => h.write(b"ERR"),
        }
    }
    (h.0, frames)
}

fn wv_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir)
    else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "wv"))
        .collect();
    files.sort();
    files
}

fn corpus_dirs() -> Vec<(String, PathBuf)> {
    let mut dirs = vec![(
        "fixtures".to_string(),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
    )];
    if let Some(samples) = std::env::var_os("RMPD_SAMPLES") {
        dirs.push(("samples".to_string(), Path::new(&samples).join("wavpack")));
    }
    dirs
}

/// `(label, hash, frames)` of every decode.
fn all_hashes() -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for (set, dir) in corpus_dirs() {
        for wv in wv_files(&dir) {
            let name = format!("{set}/{}", wv.file_name().unwrap().to_string_lossy());
            let opts = with_sibling_correction(&wv, FormatOptions::default());
            let has_wvc = opts.external_data.sidecar.is_some();
            let Ok(mut reader) = WavPackReader::try_new(mss(&wv), opts)
            else {
                continue;
            };
            let (h, n) = hash_decode(&mut reader);
            out.push((name.clone(), h, n));
            if has_wvc {
                let mut reader =
                    WavPackReader::try_new(mss(&wv), FormatOptions::default()).unwrap();
                let (h, n) = hash_decode(&mut reader);
                out.push((format!("{name}[lossy]"), h, n));
            }
        }
    }
    out
}

#[test]
fn golden_decode_hashes() {
    let hashes = all_hashes();
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        for (name, h, n) in &hashes {
            println!("    (\"{name}\", 0x{h:016x}, {n}),");
        }
        return;
    }
    let have_samples = std::env::var_os("RMPD_SAMPLES").is_some();
    for (name, h, n) in &hashes {
        let Some(g) = GOLDEN.iter().find(|g| g.0 == name)
        else {
            panic!("{name}: no golden hash (run with GOLDEN_PRINT=1)");
        };
        assert_eq!((g.1, g.2), (*h, *n), "{name}: decode differs from the golden output");
    }
    for g in GOLDEN {
        if g.0.starts_with("samples/") && !have_samples {
            continue;
        }
        assert!(hashes.iter().any(|h| h.0 == g.0), "{}: file missing", g.0);
    }
}

/// Seeks (forwards, backwards, repeatedly, to the end) land on a block boundary at or before the
/// target and decode exactly the tail of the sequential decode.
#[test]
fn seeks_match_sequential_decode() {
    for (_, dir) in corpus_dirs() {
        for wv in wv_files(&dir) {
            let opts = with_sibling_correction(&wv, FormatOptions::default());
            let Ok(mut reader) = WavPackReader::try_new(mss(&wv), opts)
            else {
                continue;
            };
            let total = reader.tracks()[0].num_frames.unwrap_or(0);
            if total == 0 {
                continue;
            }

            // The per-packet hashes with their timestamps, decoded sequentially.
            let mut decoder = make_decoder(&reader);
            let mut packets: Vec<(i64, u64)> = Vec::new();
            while let Ok(Some(p)) = reader.next_packet() {
                let mut h = Fnv::new();
                if let Ok(buf) = decoder.decode_ref(&p.as_packet_ref()) {
                    feed(&mut h, &buf);
                }
                packets.push((p.pts.get(), h.0));
            }
            assert!(!packets.is_empty());

            let t = total as i64;
            let targets =
                [t / 2, 0, t - 1, t / 3, t / 3 + 7, 1, (t * 9) / 10, t / 7, t / 2, t - 1, 5];
            for ts in targets {
                let seeked = reader
                    .seek(
                        SeekMode::Accurate,
                        SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 },
                    )
                    .unwrap_or_else(|e| panic!("{}: seek to {ts}: {e}", wv.display()));
                let landed = seeked.actual_ts.get();
                assert!(landed <= ts, "{}: landed after the target", wv.display());

                let first = packets.iter().position(|p| p.0 == landed).expect("block start");
                // Cumulative: the packet at `first` contains the target.
                if let Some(next) = packets.get(first + 1) {
                    assert!(ts < next.0, "{}: landed too early", wv.display());
                }

                let mut decoder = make_decoder(&reader);
                let mut i = first;
                // Decode a few packets (all for small files) after the seek.
                while let Ok(Some(p)) = reader.next_packet() {
                    assert_eq!(p.pts.get(), packets[i].0, "{}: timestamps", wv.display());
                    let mut h = Fnv::new();
                    if let Ok(buf) = decoder.decode_ref(&p.as_packet_ref()) {
                        feed(&mut h, &buf);
                    }
                    // A decoder that starts mid-stream is only exact when the blocks are
                    // independent, which they are in WavPack.
                    assert_eq!(h.0, packets[i].1, "{}: packet at {}", wv.display(), p.pts.get());
                    i += 1;
                    if i - first >= 4 {
                        break;
                    }
                }
            }
        }
    }
}

// Generated with `GOLDEN_PRINT=1`.
const GOLDEN: &[(&str, u64, u64)] = &[
    ("fixtures/float_hybrid_lossy.wv", 0x0294ec9d0e017807, 2205),
    ("fixtures/float_lossless.wv", 0xb68d7c356f3bae1c, 2205),
    ("fixtures/float_mono_lossless.wv", 0xe0ba3f7b0b7b54e0, 2205),
    ("fixtures/int_hybrid_lossy.wv", 0x46770d552c9457bd, 2205),
    ("fixtures/multi3ch_mixed_int16_lossless.wv", 0x8e6343aec0feda4c, 2205),
    ("fixtures/multi4ch_float32_lossless.wv", 0xd0e8ebe115002569, 2205),
    ("fixtures/multi4ch_int16_hybrid_lossy.wv", 0x7a0d9e0960d1e9db, 2205),
    ("fixtures/multi4ch_int16_lossless.wv", 0xd4aa97d959bc3bdd, 2205),
    ("fixtures/multi6ch_51_int16_lossless.wv", 0x54ab581bf56d9be5, 2205),
    ("fixtures/test_ramp_mono_fast.wv", 0x36b57055316de9db, 100),
    ("fixtures/test_ramp_mono_high.wv", 0x36b57055316de9db, 100),
    ("fixtures/test_silence_mono_fast.wv", 0x37027190f725c8c5, 100),
    ("fixtures/test_silence_mono_high.wv", 0x37027190f725c8c5, 100),
    ("fixtures/test_silence_stereo_fast.wv", 0x2c1b93daafb34265, 100),
    ("fixtures/test_silence_stereo_high.wv", 0x2c1b93daafb34265, 100),
    ("fixtures/wvc_float_mono.wv", 0x40e04b3d9bf8aebc, 3000),
    ("fixtures/wvc_float_mono.wv[lossy]", 0x6a51482ee09c00ef, 3000),
    ("fixtures/wvc_float_stereo.wv", 0xb22aa21f7e9413fc, 3000),
    ("fixtures/wvc_float_stereo.wv[lossy]", 0xe6913c72ce39df9c, 3000),
    ("fixtures/wvc_int16_6ch.wv", 0x29efea9cb2713a61, 2200),
    ("fixtures/wvc_int16_6ch.wv[lossy]", 0xd2ed168fbf967d51, 2200),
    ("fixtures/wvc_int16_mono.wv", 0x0adf0ee27701a698, 5000),
    ("fixtures/wvc_int16_mono.wv[lossy]", 0xce17200c39d053f9, 5000),
    ("fixtures/wvc_int16_quad.wv", 0x5cc803a8a60315c6, 2500),
    ("fixtures/wvc_int16_quad.wv[lossy]", 0x14c423f407b1114c, 2500),
    ("fixtures/wvc_int16_stereo.wv", 0xb11e70cbcbe0765d, 5000),
    ("fixtures/wvc_int16_stereo.wv[lossy]", 0xa8d0305bd48b4e99, 5000),
    ("fixtures/wvc_int16_stereo_cc.wv", 0x8ed247c12c422754, 3000),
    ("fixtures/wvc_int16_stereo_cc.wv[lossy]", 0x56ec5ea15caeb183, 3000),
    ("fixtures/wvc_int16_stereo_cross.wv", 0x604267e6ded3e0f1, 3000),
    ("fixtures/wvc_int16_stereo_cross.wv[lossy]", 0x709ce67fbd388ac2, 3000),
    ("fixtures/wvc_int16_stereo_kbps.wv", 0x75845114743a2078, 3000),
    ("fixtures/wvc_int16_stereo_kbps.wv[lossy]", 0xfff0cc203c21b059, 3000),
    ("fixtures/wvc_int16_stereo_lr.wv", 0x0477513545d4fffe, 3000),
    ("fixtures/wvc_int16_stereo_lr.wv[lossy]", 0x049bb10f057937ec, 3000),
    ("fixtures/wvc_int16_stereo_noshape.wv", 0xe73ae83d2d41ea46, 3000),
    ("fixtures/wvc_int16_stereo_noshape.wv[lossy]", 0x509b8084b8a6bc95, 3000),
    ("fixtures/wvc_int24_stereo.wv", 0xd1d546927811bde7, 3000),
    ("fixtures/wvc_int24_stereo.wv[lossy]", 0x4637d2034e07fee4, 3000),
    ("fixtures/wvc_int32_stereo.wv", 0x7e1c91db7e1e7fe6, 3000),
    ("fixtures/wvc_int32_stereo.wv[lossy]", 0x49f442aed0a81bde, 3000),
    ("fixtures/wvc_int8_stereo.wv", 0x46c5926c58704cb8, 3000),
    ("fixtures/wvc_int8_stereo.wv[lossy]", 0x01725271a88e1284, 3000),
    ("samples/wv_24bit_192k.wv", 0x29d87f991088fe7d, 3840000),
    ("samples/wv_24bit_96k.wv", 0xaa0e7aab894af371, 1920000),
    ("samples/wv_32bit.wv", 0x3266af969814018b, 1920000),
    ("samples/wv_3ch.wv", 0x7920d8b0b9ffdfd5, 1323000),
    ("samples/wv_6ch.wv", 0x59f82f0c1a03f357, 1323000),
    ("samples/wv_8bit.wv", 0xa04ae0fc2466a80b, 1323000),
    ("samples/wv_8ch.wv", 0x0d25338fedbf58e5, 1323000),
    ("samples/wv_8k_mono.wv", 0x4c3c9176785c1f41, 240000),
    ("samples/wv_extra6.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_fast.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_ffmpeg.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_ffmpeg_6ch.wv", 0x59f82f0c1a03f357, 1323000),
    ("samples/wv_float32.wv", 0x9902c30031fdd547, 1920000),
    ("samples/wv_high.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_hybrid_24bit.wv", 0x89e99d88b893ba60, 1920000),
    ("samples/wv_hybrid_256.wv", 0x50cd5a9f79c3bcfe, 1323000),
    ("samples/wv_hybrid_6ch.wv", 0x69860c9cfad3744a, 1323000),
    ("samples/wv_hybrid_96.wv", 0x1fe1944e20fae779, 1323000),
    ("samples/wv_hybrid_noshaping.wv", 0xd420961d408338ab, 1323000),
    ("samples/wv_hybrid_with_wvc.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_hybrid_with_wvc.wv[lossy]", 0x6e13ba93569f378e, 1323000),
    ("samples/wv_joint_off.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_joint_on.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_lossless.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_md5.wv", 0x4d4f64e82894e21d, 1323000),
    ("samples/wv_mono.wv", 0x02f2944c3ba19949, 1323000),
    ("samples/wv_vhigh.wv", 0x4d4f64e82894e21d, 1323000),
];
