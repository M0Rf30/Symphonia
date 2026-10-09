// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for hybrid-lossless decoding of a WavPack `.wv` file together with its
//! `.wvc` correction file.
//!
//! The `wvc_*.wv` / `wvc_*.wvc` fixtures were generated with the reference `wavpack`
//! (dbry/WavPack 5.9.0, BSD-3-Clause) from short synthetic sources (tones plus noise) with
//! `--blocksize=2048`, e.g. `wavpack -b3 -c --blocksize=2048 src.wav -o wvc_int16_stereo.wv`.
//! The `wvc_*_ref.raw` files are the raw little-endian PCM (or float) of the *source*: `wvunpack`
//! of the pair reproduces them exactly, so they are the bit-exactness oracle.
//!
//! The fixtures cover every hybrid-lossless code path: mono and stereo, joint stereo on and off,
//! cross-channel decorrelation, noise shaping on and off (and "new" negative shaping), bitrate
//! (kbps) and bits/sample modes, 8, 16, 24 and 32-bit integers, 32-bit float, multi-block
//! files and multichannel files.
//!
//! Tests that need the (large) sample corpus are gated on `RMPD_SAMPLES`.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia_codec_wavpack::{
    WavPackDecoder, WavPackReader, correction_path, with_sibling_correction,
};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, ReadOnlySource};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn mss(path: &Path) -> MediaSourceStream<'static> {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    MediaSourceStream::new(Box::new(file), Default::default())
}

fn mss_bytes(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default())
}

fn append_buf(out: &mut Vec<u8>, buf: &GenericAudioBufferRef<'_>) {
    let n = buf.frames();
    let nch = buf.spec().channels().count();
    macro_rules! interleave {
        ($b:expr, $conv:expr) => {
            for i in 0..n {
                for ch in 0..nch {
                    out.extend_from_slice(&$conv($b.plane(ch).unwrap()[i]));
                }
            }
        };
    }
    match buf {
        // WAV 8-bit PCM is unsigned.
        GenericAudioBufferRef::S8(b) => interleave!(b, |v: i8| [(v as u8) ^ 0x80]),
        GenericAudioBufferRef::S16(b) => interleave!(b, |v: i16| v.to_le_bytes()),
        GenericAudioBufferRef::S24(b) => {
            interleave!(b, |v: symphonia_core::audio::sample::i24| v.to_le_bytes())
        }
        GenericAudioBufferRef::S32(b) => interleave!(b, |v: i32| v.to_le_bytes()),
        GenericAudioBufferRef::F32(b) => interleave!(b, |v: f32| v.to_le_bytes()),
        _ => panic!("unhandled sample format in test harness"),
    }
}

fn make_decoder(reader: &dyn FormatReader) -> WavPackDecoder {
    let params: AudioCodecParameters = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("no audio codec params"),
    };
    WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder")
}

/// Decode every remaining packet, returning the raw bytes of each packet.
fn decode_blocks(reader: &mut dyn FormatReader) -> Vec<Vec<u8>> {
    let mut decoder = make_decoder(reader);
    let mut blocks = Vec::new();
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let buf = decoder.decode_ref(&packet.as_packet_ref()).expect("decode");
        let mut out = Vec::new();
        append_buf(&mut out, &buf);
        blocks.push(out);
    }
    blocks
}

fn decode_all(reader: &mut dyn FormatReader) -> Vec<u8> {
    decode_blocks(reader).concat()
}

fn wv_with_wvc(name: &str) -> WavPackReader<'static> {
    WavPackReader::try_new_with_correction(
        mss(&fixture(&format!("wvc_{name}.wv"))),
        mss(&fixture(&format!("wvc_{name}.wvc"))),
        FormatOptions::default(),
    )
    .expect("reader with correction")
}

fn wv_alone(name: &str) -> WavPackReader<'static> {
    WavPackReader::try_new(mss(&fixture(&format!("wvc_{name}.wv"))), FormatOptions::default())
        .expect("reader")
}

fn reference(name: &str) -> Vec<u8> {
    std::fs::read(fixture(&format!("wvc_{name}_ref.raw"))).expect("read reference raw")
}

const CASES: &[&str] = &[
    "int16_stereo",       // joint stereo, shaping, bitrate + balance, 3 blocks
    "int16_stereo_lr",    // joint stereo off
    "int16_stereo_cc",    // `-cc`: cross-channel decorrelation + negative ("new") shaping
    "int16_stereo_cross", // `--cross-decorr`
    "int16_stereo_noshape",
    "int16_stereo_kbps", // kbps bitrate mode
    "int16_mono",
    "int8_stereo",
    "int24_stereo",
    "int32_stereo", // INT32_DATA
    "float_stereo", // wvx bitstream in the correction file
    "float_mono",
    "int16_quad", // 2 streams
    "int16_6ch",  // 4 streams (2 mono + 2 stereo)
];

#[test]
fn lossless_with_wvc_is_bit_exact_vs_source() {
    for name in CASES {
        let mut reader = wv_with_wvc(name);
        assert!(reader.has_correction());
        let decoded = decode_all(&mut reader);
        let reference = reference(name);
        assert_eq!(decoded.len(), reference.len(), "{name}: decoded length mismatch");
        assert!(decoded == reference, "{name}: decoded bytes differ from the lossless source");
    }
}

#[test]
fn without_wvc_the_lossy_core_is_decoded() {
    for name in CASES {
        let mut reader = wv_alone(name);
        assert!(!reader.has_correction());
        let lossy = decode_all(&mut reader);
        let reference = reference(name);
        assert_eq!(lossy.len(), reference.len(), "{name}: decoded length mismatch");
        // At these bitrates the lossy core is audibly different from the source.
        assert!(lossy != reference, "{name}: lossy decode unexpectedly equals the lossless source");
    }
}

#[test]
fn sidecar_in_format_options_enables_lossless() {
    for name in ["int16_stereo", "float_stereo", "int16_6ch"] {
        let wvc = File::open(fixture(&format!("wvc_{name}.wvc"))).unwrap();
        let opts = FormatOptions::default().sidecar(Box::new(wvc));
        let mut reader =
            WavPackReader::try_new(mss(&fixture(&format!("wvc_{name}.wv"))), opts).expect("reader");
        assert!(reader.has_correction());
        assert!(decode_all(&mut reader) == reference(name), "{name}");
    }
}

#[test]
fn sidecar_through_the_probe() {
    let mut probe = Probe::default();
    probe.register_format::<WavPackReader<'_>>();

    let path = fixture("wvc_int16_stereo.wv");
    let opts = with_sibling_correction(&path, FormatOptions::default());
    let mut reader =
        probe.probe(&Hint::new(), mss(&path), opts, MetadataOptions::default()).expect("probe");

    assert!(decode_all(reader.as_mut()) == reference("int16_stereo"));

    // Without the helper there is no correction.
    let mut reader = probe
        .probe(&Hint::new(), mss(&path), FormatOptions::default(), MetadataOptions::default())
        .expect("probe");
    assert!(decode_all(reader.as_mut()) != reference("int16_stereo"));
}

#[test]
fn correction_path_finds_the_sibling() {
    assert_eq!(
        correction_path(&fixture("wvc_int16_stereo.wv")),
        Some(fixture("wvc_int16_stereo.wvc"))
    );
    // A lossless (or plain lossy) file has no sibling.
    assert_eq!(correction_path(&fixture("float_lossless.wv")), None);
    assert_eq!(correction_path(Path::new("/nonexistent/dir/file.wv")), None);

    // Options are unchanged when there is no sibling.
    let opts = with_sibling_correction(&fixture("float_lossless.wv"), FormatOptions::default());
    assert!(opts.external_data.sidecar.is_none());
}

#[test]
fn unusable_correction_stream() {
    // An explicit correction stream that is garbage is an error ...
    let result = WavPackReader::try_new_with_correction(
        mss(&fixture("wvc_int16_stereo.wv")),
        mss_bytes(vec![0x55; 4096]),
        FormatOptions::default(),
    );
    assert!(result.is_err());

    // ... a garbage sidecar is ignored (the lossy audio is decoded).
    let opts = FormatOptions::default().sidecar(Box::new(std::io::Cursor::new(vec![0x55u8; 4096])));
    let mut reader = WavPackReader::try_new(mss(&fixture("wvc_int16_stereo.wv")), opts).unwrap();
    assert!(!reader.has_correction());
    assert_eq!(decode_all(&mut reader), decode_all(&mut wv_alone("int16_stereo")));

    // ... and so is a sidecar for a file that has no use for it.
    let wvc = File::open(fixture("wvc_int16_stereo.wvc")).unwrap();
    let opts = FormatOptions::default().sidecar(Box::new(wvc));
    let mut reader = WavPackReader::try_new(mss(&fixture("float_lossless.wv")), opts).unwrap();
    let plain = decode_all(
        &mut WavPackReader::try_new(mss(&fixture("float_lossless.wv")), FormatOptions::default())
            .unwrap(),
    );
    assert_eq!(decode_all(&mut reader), plain);
}

/// The byte ranges of the blocks of a WavPack file: `(start, end)`.
fn block_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut pos = 0;
    while pos + 32 <= data.len() && &data[pos..pos + 4] == b"wvpk" {
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize + 8;
        blocks.push((pos, pos + size));
        pos += size;
    }
    blocks
}

/// The byte range of the data of the sub-block `id` in the block `block`.
fn sub_block_data(data: &[u8], block: (usize, usize), id: u8) -> Option<(usize, usize)> {
    let mut pos = block.0 + 32;
    while pos + 2 <= block.1 {
        let sid = data[pos];
        let (len, hdr) = if sid & 0x80 != 0 {
            (u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], 0]) as usize * 2, 4)
        }
        else {
            (data[pos + 1] as usize * 2, 2)
        };
        if sid & 0x3f == id {
            return Some((pos + hdr, pos + hdr + len));
        }
        pos += hdr + len;
    }
    None
}

#[test]
fn corrupt_correction_block_falls_back_to_lossy_for_that_block_only() {
    let name = "int16_stereo";
    let reference = reference(name);
    let lossy_blocks = decode_blocks(&mut wv_alone(name));
    assert_eq!(lossy_blocks.len(), 3);

    let mut wvc = std::fs::read(fixture(&format!("wvc_{name}.wvc"))).unwrap();
    let blocks = block_ranges(&wvc);
    assert_eq!(blocks.len(), 3);

    // Flip a bit in the middle of the correction bitstream of the second block.
    let (start, end) = sub_block_data(&wvc, blocks[1], 0x0b).expect("correction bitstream");
    wvc[(start + end) / 2] ^= 0x10;

    let mut reader = WavPackReader::try_new_with_correction(
        mss(&fixture(&format!("wvc_{name}.wv"))),
        mss_bytes(wvc),
        FormatOptions::default(),
    )
    .unwrap();
    let blocks = decode_blocks(&mut reader);
    assert_eq!(blocks.len(), 3);

    let first = lossy_blocks[0].len();
    let second = first + lossy_blocks[1].len();
    let all = blocks.concat();
    assert!(all[..first] == reference[..first], "first block must stay lossless");
    assert!(all[second..] == reference[second..], "last block must stay lossless");
    assert!(blocks[1] != reference[first..second], "the corrupt block cannot be lossless");
    assert!(blocks[1] == lossy_blocks[1], "the corrupt block must decode as the lossy core");
}

#[test]
fn truncated_correction_stream_decodes_the_rest_lossy() {
    let name = "int16_stereo";
    let reference = reference(name);
    let lossy_blocks = decode_blocks(&mut wv_alone(name));

    let wvc = std::fs::read(fixture(&format!("wvc_{name}.wvc"))).unwrap();
    let blocks = block_ranges(&wvc);
    // Cut in the middle of the last block (an unreadable block) ...
    for end in [blocks[2].0, (blocks[2].0 + blocks[2].1) / 2] {
        let mut reader = WavPackReader::try_new_with_correction(
            mss(&fixture(&format!("wvc_{name}.wv"))),
            mss_bytes(wvc[..end].to_vec()),
            FormatOptions::default(),
        )
        .unwrap();
        let decoded = decode_blocks(&mut reader);
        assert_eq!(decoded.len(), 3);
        let split = lossy_blocks[0].len() + lossy_blocks[1].len();
        assert!(decoded.concat()[..split] == reference[..split]);
        assert!(decoded[2] == lossy_blocks[2]);
    }
}

#[test]
fn mismatched_correction_stream_is_ignored() {
    // The correction file of another file, with a different stream layout.
    let mut reader = WavPackReader::try_new_with_correction(
        mss(&fixture("wvc_int16_stereo.wv")),
        mss(&fixture("wvc_int16_stereo_lr.wvc")),
        FormatOptions::default(),
    )
    .unwrap();
    assert_eq!(decode_all(&mut reader), decode_all(&mut wv_alone("int16_stereo")));

    // A correction file that starts later than the main file (leading blocks missing) only
    // loses those blocks.
    let wvc = std::fs::read(fixture("wvc_int16_stereo.wvc")).unwrap();
    let blocks = block_ranges(&wvc);
    let mut reader = WavPackReader::try_new_with_correction(
        mss(&fixture("wvc_int16_stereo.wv")),
        mss_bytes(wvc[blocks[1].0..].to_vec()),
        FormatOptions::default(),
    )
    .unwrap();
    let decoded = decode_blocks(&mut reader);
    let lossy = decode_blocks(&mut wv_alone("int16_stereo"));
    let reference = reference("int16_stereo");
    let first = 2048 * 4;
    assert!(decoded[0] == lossy[0]);
    assert!(decoded[1..].concat() == reference[first..]);
}

#[test]
fn seeking_keeps_the_correction_in_sync() {
    let name = "int16_stereo";
    let reference = reference(name);
    let mut reader = wv_with_wvc(name);

    // Read ahead first so the correction stream has to be rewound.
    let _ = decode_blocks(&mut reader);

    for ts in [0u64, 100, 2048, 2049, 4500, 3000, 1] {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(ts as i64), track_id: 0 },
            )
            .expect("seek");
        assert!(seeked.actual_ts.get() as u64 <= ts);

        let tail = decode_all(&mut reader);
        let from = seeked.actual_ts.get() as usize * 4;
        assert!(tail == reference[from..], "seek to {ts} (landed at {})", seeked.actual_ts.get());
    }
}

#[test]
fn seeking_without_a_seekable_correction_stream_fails_cleanly() {
    let wvc = File::open(fixture("wvc_int16_stereo.wvc")).unwrap();
    let wvc = MediaSourceStream::new(Box::new(ReadOnlySource::new(wvc)), Default::default());
    let mut reader = WavPackReader::try_new_with_correction(
        mss(&fixture("wvc_int16_stereo.wv")),
        wvc,
        FormatOptions::default(),
    )
    .unwrap();

    // Reading from start to end works.
    assert!(decode_all(&mut reader) == reference("int16_stereo"));

    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(10), track_id: 0 })
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Sample corpus (gated on `RMPD_SAMPLES`)
// ---------------------------------------------------------------------------

/// The PCM payload of a canonical little-endian RIFF/WAVE file.
fn wav_data(path: &Path) -> Vec<u8> {
    let data = std::fs::read(path).unwrap();
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return data[pos + 8..(pos + 8 + size).min(data.len())].to_vec();
        }
        pos += 8 + size + (size & 1);
    }
    panic!("no data chunk in {}", path.display());
}

#[test]
fn sample_hybrid_with_wvc_is_bit_exact() {
    let Some(samples) = std::env::var_os("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set, skipping");
        return;
    };
    let wv = Path::new(&samples).join("wavpack/wv_hybrid_with_wvc.wv");
    if !wv.exists() {
        eprintln!("{} not found, skipping", wv.display());
        return;
    }
    assert!(correction_path(&wv).is_some());

    let mut reader = WavPackReader::try_new_with_correction(
        mss(&wv),
        mss(&wv.with_extension("wvc")),
        FormatOptions::default(),
    )
    .unwrap();
    let decoded = decode_all(&mut reader);

    // Versus the lossless source the pair was made from ...
    let source = Path::new(&samples).join("../refs/r16_44100_2.wav");
    if source.exists() {
        let source = wav_data(&source);
        assert_eq!(decoded.len(), source.len());
        assert!(decoded == source, "differs from the lossless source");
    }

    // ... and versus `wvunpack`, which applies the correction file automatically.
    let out = std::env::temp_dir().join(format!("wvc_sample_{}.raw", std::process::id()));
    match std::process::Command::new("wvunpack")
        .args(["-q", "-y", "-r"])
        .arg(&wv)
        .arg("-o")
        .arg(&out)
        .status()
    {
        Ok(status) if status.success() => {
            let oracle = std::fs::read(&out).unwrap();
            let _ = std::fs::remove_file(&out);
            assert_eq!(decoded.len(), oracle.len());
            assert!(decoded == oracle, "differs from wvunpack");
        }
        _ => eprintln!("wvunpack not available, skipping the oracle comparison"),
    }

    // Without the correction file the same file decodes to the lossy core.
    let lossy =
        decode_all(&mut WavPackReader::try_new(mss(&wv), FormatOptions::default()).unwrap());
    assert_eq!(lossy.len(), decoded.len());
    assert!(lossy != decoded);
}

/// Every WavPack sample of the corpus, decoded the way `wvunpack` does (with the sibling `.wvc`
/// file if there is one, through the convenience helper), is bit-identical to `wvunpack`.
#[test]
fn sample_corpus_matches_wvunpack() {
    let Some(samples) = std::env::var_os("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set, skipping");
        return;
    };
    let dir = Path::new(&samples).join("wavpack");
    let Ok(entries) = std::fs::read_dir(&dir)
    else {
        eprintln!("{} not found, skipping", dir.display());
        return;
    };
    if std::process::Command::new("wvunpack").arg("--version").output().is_err() {
        eprintln!("wvunpack not available, skipping");
        return;
    }

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "wv"))
        .collect();
    files.sort();
    assert!(!files.is_empty());

    let mut with_correction = 0;
    for wv in &files {
        let opts = with_sibling_correction(wv, FormatOptions::default());
        with_correction += opts.external_data.sidecar.is_some() as usize;

        let mut reader = WavPackReader::try_new(mss(wv), opts).unwrap();
        let decoded = decode_all(&mut reader);

        let out = std::env::temp_dir().join(format!(
            "wvc_corpus_{}_{}.raw",
            std::process::id(),
            wv.file_stem().unwrap().to_string_lossy()
        ));
        let status = std::process::Command::new("wvunpack")
            .args(["-q", "-y", "-r"])
            .arg(wv)
            .arg("-o")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "wvunpack failed on {}", wv.display());
        let oracle = std::fs::read(&out).unwrap();
        let _ = std::fs::remove_file(&out);

        assert_eq!(decoded.len(), oracle.len(), "{}: length", wv.display());
        assert!(decoded == oracle, "{}: differs from wvunpack", wv.display());
    }
    assert!(with_correction >= 1);
}

/// Decode, tolerating (but not panicking on) errors.
fn decode_tolerant(reader: &mut dyn FormatReader) {
    let mut decoder = make_decoder(reader);
    while let Ok(Some(packet)) = reader.next_packet() {
        let _ = decoder.decode_ref(&packet.as_packet_ref());
    }
}

#[test]
fn mutated_correction_streams_never_panic() {
    // A small xorshift generator keeps the test deterministic.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for name in ["int16_stereo", "int16_mono", "float_stereo", "int32_stereo", "int16_6ch"] {
        let wvc = std::fs::read(fixture(&format!("wvc_{name}.wvc"))).unwrap();

        for round in 0..150 {
            let mut data = wvc.clone();
            for _ in 0..1 + round % 4 {
                let pos = (next() as usize) % data.len();
                data[pos] = next() as u8;
            }
            if round % 5 == 0 {
                data.truncate((next() as usize) % data.len());
            }

            // The correction stream may be rejected up front; if not it must never crash the
            // decode.
            if let Ok(mut reader) = WavPackReader::try_new_with_correction(
                mss(&fixture(&format!("wvc_{name}.wv"))),
                mss_bytes(data),
                FormatOptions::default(),
            ) {
                decode_tolerant(&mut reader);
            }
        }
    }
}

#[test]
fn mutated_main_streams_with_correction_never_panic() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for name in ["int16_stereo", "float_mono", "int32_stereo", "int16_quad"] {
        let wv = std::fs::read(fixture(&format!("wvc_{name}.wv"))).unwrap();

        for round in 0..150 {
            let mut data = wv.clone();
            for _ in 0..1 + round % 4 {
                let pos = (next() as usize) % data.len();
                data[pos] = next() as u8;
            }

            if let Ok(mut reader) = WavPackReader::try_new_with_correction(
                mss_bytes(data),
                mss(&fixture(&format!("wvc_{name}.wvc"))),
                FormatOptions::default(),
            ) {
                decode_tolerant(&mut reader);
            }
        }
    }
}
