// CELT-only RFC 8251 conformance harness (wave 1, owner: "CeltSynthesis").
//
// Drives `crate::celt::CeltDecoder` directly (mirroring the CELT-relevant slice of
// `opus_decode_frame`/`opus_decode_native` from `src/opus_decoder.c`), bypassing the
// not-yet-implemented top-level `OpusDecoder`/multistream (wave 2's job, see
// `tests/conformance.rs`). Only exercises vectors that are pure CELT-only (no SILK/Hybrid
// frames): 01, 07, 11 per the wave-1 assignment.
//
// Run with:
//   OPUS_TESTVECTORS=/home/gianluca/M0Rf30/wt/ref/opus_newvectors \
//     cargo test -p symphonia-codec-opus --release --test celt_vectors -- --ignored --nocapture

mod common;

use common::opus_demo::BitFile;
use symphonia_codec_opus::celt::CeltDecoder;
use symphonia_codec_opus::packet::{self, Bandwidth, OpusMode};

fn testvectors_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("OPUS_TESTVECTORS").map(std::path::PathBuf::from)
}

fn read_dec_as_f32(path: &std::path::Path, channels: usize) -> Vec<f32> {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    assert_eq!(raw.len() % 2, 0);
    let samples: Vec<f32> = raw
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
        .collect();
    assert_eq!(samples.len() % channels, 0);
    samples
}

/// C: `opus_decoder.c`'s `endband` switch in `opus_decode_frame` (`OPUS_BANDWIDTH_*` ->
/// `CELT_SET_END_BAND`). `start` is always `0` for CELT-only frames (only Hybrid sets `17`).
fn end_band_for_bandwidth(bw: Bandwidth) -> i32 {
    match bw {
        Bandwidth::Narrowband => 13,
        Bandwidth::Mediumband | Bandwidth::Wideband => 17,
        Bandwidth::Superwideband => 19,
        Bandwidth::Fullband => 21,
    }
}

struct DecodeStats {
    pcm: Vec<f32>,
    range_mismatches: usize,
    frames_checked: usize,
}

/// Decodes an entire CELT-only `.bit` vector at the given output channel count directly through
/// [`CeltDecoder`], returning interleaved `f32` PCM (in `i16` range, matching `.dec`/`m.dec`) plus
/// the count of packets whose decoder final range mismatched the encoder's recorded final range.
///
/// Lost packets reuse the most recently decoded frame's size for PLC continuity (the `.bit`
/// format doesn't record a size for lost packets; a real streaming caller would keep requesting
/// the same frame duration it was already using).
fn decode_celt_vector(bit_path: &std::path::Path, channels: u8) -> DecodeStats {
    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut decoder = CeltDecoder::new(48000, channels);
    let mut pcm = Vec::new();
    let mut range_mismatches = 0usize;
    let mut frames_checked = 0usize;
    let mut last_frame_size = 960i32; // 20 ms @ 48 kHz, matching the largest CELT frame.

    for pkt in bitfile.iter() {
        if pkt.lost {
            let mut out = vec![0f32; last_frame_size as usize * channels as usize];
            let n = decoder.decode_with_ec(None, &mut out, last_frame_size, None, false).unwrap();
            pcm.extend_from_slice(&out[..n * channels as usize]);
            continue;
        }

        let parsed = packet::parse(&pkt.payload).expect("valid CELT-only packet");
        assert_eq!(
            parsed.toc.mode(),
            OpusMode::CeltOnly,
            "celt_vectors.rs only decodes CELT-only packets (found {:?})",
            parsed.toc.mode()
        );

        let end = end_band_for_bandwidth(parsed.toc.bandwidth());
        decoder.set_start_band(0);
        decoder.set_end_band(end);
        decoder.set_channels(if parsed.toc.stereo() { 2 } else { 1 });

        let frame_size = parsed.toc.samples_per_frame(48000) as i32;
        last_frame_size = frame_size;

        for frame in &parsed.frames {
            let frame_data = &pkt.payload[frame.offset..frame.offset + frame.len];
            let mut out = vec![0f32; frame_size as usize * channels as usize];
            let n = decoder.decode_with_ec(Some(frame_data), &mut out, frame_size, None, false).unwrap();
            pcm.extend_from_slice(&out[..n * channels as usize]);
        }

        frames_checked += 1;
        if decoder.final_range() != pkt.enc_final_range {
            range_mismatches += 1;
        }
    }

    DecodeStats { pcm, range_mismatches, frames_checked }
}

macro_rules! celt_conformance_test {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            let Some(dir) = testvectors_dir()
            else {
                eprintln!("OPUS_TESTVECTORS not set; skipping (see module docs for the command)");
                return;
            };
            let bit_path = dir.join(format!("testvector{:02}.bit", $index));
            if !bit_path.exists() {
                eprintln!("{bit_path:?} not found; skipping");
                return;
            }

            // Stereo cross-check.
            let stats = decode_celt_vector(&bit_path, 2);
            assert_eq!(
                stats.range_mismatches, 0,
                "final range mismatches (stereo): {}/{} packets",
                stats.range_mismatches, stats.frames_checked
            );
            let reference = read_dec_as_f32(&dir.join(format!("testvector{:02}.dec", $index)), 2);
            let result = common::opus_compare::compare(&reference, &stats.pcm, 2);
            assert!(result.pass, "opus_compare FAILS (stereo): {result:?}");

            // Mono cross-check (exercises CeltDecoder's stereo->mono downmix in celt_synthesis
            // when the underlying packets are stereo-encoded).
            let stats_m = decode_celt_vector(&bit_path, 1);
            assert_eq!(
                stats_m.range_mismatches, 0,
                "final range mismatches (mono): {}/{} packets",
                stats_m.range_mismatches, stats_m.frames_checked
            );
            let reference_m = read_dec_as_f32(&dir.join(format!("testvector{:02}m.dec", $index)), 1);
            let result_m = common::opus_compare::compare(&reference_m, &stats_m.pcm, 1);
            assert!(result_m.pass, "opus_compare FAILS (mono): {result_m:?}");
        }
    };
}

celt_conformance_test!(vector01, 1);
celt_conformance_test!(vector07, 7);
celt_conformance_test!(vector11, 11);
