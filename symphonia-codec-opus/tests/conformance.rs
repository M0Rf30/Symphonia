// RFC 8251 conformance harness for the Opus decoder.
//
// Run with (once the decoder is implemented in a later wave):
//   OPUS_TESTVECTORS=/home/gianluca/M0Rf30/wt/ref/opus_newvectors \
//     cargo test -p symphonia-codec-opus --release -- --ignored
//
// Wave 0 leaves the actual decode tests `#[ignore]`d (they call into `todo!()` decoder methods
// and would panic); they exist so wave 2 has a stable acceptance target. The mode/bandwidth
// survey test (`survey_test_vectors`) is NOT ignored: it exercises only `crate::packet` (fully
// implemented in wave 0) and is useful for wave-1 SILK/CELT agents to pick vectors that isolate
// specific coding modes.

mod common;

use common::opus_demo::BitFile;
use symphonia_codec_opus::packet::{self, OpusMode};

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

/// Walks a vector's packets and reports, per packet, the TOC-derived mode/bandwidth/frame
/// count/size and stereo flag — a debugging aid for wave-1 agents choosing vectors that isolate
/// SILK-only, Hybrid, or CELT-only decoding, or specific bandwidths.
fn survey(bit_path: &std::path::Path) -> String {
    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut mode_counts = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut bandwidth_counts = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut stereo_packets = 0usize;
    let mut lost_packets = 0usize;
    let mut total_frames = 0usize;
    let mut frame_size_set = std::collections::BTreeSet::<u32>::new();

    for pkt in bitfile.iter() {
        if pkt.lost {
            lost_packets += 1;
            continue;
        }
        let parsed = match packet::parse(&pkt.payload) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let mode_name = match parsed.toc.mode() {
            OpusMode::SilkOnly => "silk",
            OpusMode::Hybrid => "hybrid",
            OpusMode::CeltOnly => "celt",
        };
        *mode_counts.entry(mode_name).or_default() += 1;
        let bw_name = match parsed.toc.bandwidth() {
            packet::Bandwidth::Narrowband => "nb",
            packet::Bandwidth::Mediumband => "mb",
            packet::Bandwidth::Wideband => "wb",
            packet::Bandwidth::Superwideband => "swb",
            packet::Bandwidth::Fullband => "fb",
        };
        *bandwidth_counts.entry(bw_name).or_default() += 1;
        if parsed.toc.stereo() {
            stereo_packets += 1;
        }
        total_frames += parsed.frames.len();
        frame_size_set.insert(parsed.toc.samples_per_frame(48000));
    }

    format!(
        "{}: {} packets ({} lost), modes={:?}, bandwidths={:?}, stereo_packets={}, total_frames={}, frame_sizes={:?}",
        bit_path.file_name().unwrap().to_string_lossy(),
        bitfile.len(),
        lost_packets,
        mode_counts,
        bandwidth_counts,
        stereo_packets,
        total_frames,
        frame_size_set,
    )
}

#[test]
fn survey_test_vectors() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping survey (see module docs for the command)");
        return;
    };
    for i in 1..=12 {
        let bit_path = dir.join(format!("testvector{i:02}.bit"));
        if !bit_path.exists() {
            continue;
        }
        println!("{}", survey(&bit_path));
    }
}

/// Decodes an entire `.bit` vector at the given channel count through the (future) single-stream
/// decoder API, returning interleaved `i16`-range `f32` PCM plus the count of packets whose
/// decoder final range mismatched the encoder's recorded final range (`enc_final_range`) — the
/// standard RFC 8251 bit-exactness cross-check, independent of `opus_compare`'s perceptual
/// metric.
fn decode_vector(bit_path: &std::path::Path, channels: u8) -> (Vec<f32>, usize) {
    use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};

    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut decoder = OpusDecoder::try_new(SampleRate::Hz48000, channels).unwrap();
    let mut pcm = Vec::new();
    let mut range_mismatches = 0usize;

    for pkt in bitfile.iter() {
        let frame_size = 48000usize / 50; // Worst case; real code sizes from the TOC.
        let mut out = vec![0f32; frame_size * channels as usize];
        let data_opt = if pkt.lost { None } else { Some(pkt.payload.as_slice()) };
        let n = decoder.decode(data_opt, &mut out, frame_size).unwrap();
        pcm.extend_from_slice(&out[..n * channels as usize]);
        if !pkt.lost && decoder.final_range() != pkt.enc_final_range {
            range_mismatches += 1;
        }
    }

    (pcm, range_mismatches)
}

macro_rules! conformance_test {
    ($name:ident, $index:expr) => {
        #[test]
        #[ignore = "wave 2: requires OpusDecoder::decode to be implemented"]
        fn $name() {
            let Some(dir) = testvectors_dir()
            else {
                eprintln!("OPUS_TESTVECTORS not set; skipping");
                return;
            };
            let bit_path = dir.join(format!("testvector{:02}.bit", $index));

            // Stereo cross-check.
            let (pcm, mismatches) = decode_vector(&bit_path, 2);
            assert_eq!(mismatches, 0, "final range mismatches (stereo)");
            let reference = read_dec_as_f32(&dir.join(format!("testvector{:02}.dec", $index)), 2);
            let result = common::opus_compare::compare(&reference, &pcm, 2);
            assert!(result.pass, "opus_compare FAILS (stereo): {result:?}");

            // Mono cross-check.
            let (pcm_m, mismatches_m) = decode_vector(&bit_path, 1);
            assert_eq!(mismatches_m, 0, "final range mismatches (mono)");
            let reference_m = read_dec_as_f32(&dir.join(format!("testvector{:02}m.dec", $index)), 1);
            let result_m = common::opus_compare::compare(&reference_m, &pcm_m, 1);
            assert!(result_m.pass, "opus_compare FAILS (mono): {result_m:?}");
        }
    };
}

conformance_test!(vector01, 1);
conformance_test!(vector02, 2);
conformance_test!(vector03, 3);
conformance_test!(vector04, 4);
conformance_test!(vector05, 5);
conformance_test!(vector06, 6);
conformance_test!(vector07, 7);
conformance_test!(vector08, 8);
conformance_test!(vector09, 9);
conformance_test!(vector10, 10);
conformance_test!(vector11, 11);
conformance_test!(vector12, 12);

/// Validates the [`common::opus_compare`] port independent of the (unimplemented) decoder: a
/// `.dec` file must compare equal to itself, and a mildly corrupted copy must fail.
#[test]
fn opus_compare_self_test() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping opus_compare self-test");
        return;
    };
    let path = dir.join("testvector01.dec");
    if !path.exists() {
        eprintln!("testvector01.dec not found under OPUS_TESTVECTORS; skipping");
        return;
    }
    let reference = read_dec_as_f32(&path, 2);

    let self_result = common::opus_compare::compare(&reference, &reference, 2);
    assert!(self_result.pass, "identical signal must compare as PASS: {self_result:?}");
    assert!(
        self_result.weighted_error < 1e-6,
        "identical signal should have ~zero weighted error, got {}",
        self_result.weighted_error
    );

    // Corrupt a copy by zeroing every 100th sample (a highly audible click every ~2ms).
    let mut corrupted = reference.clone();
    for i in (0..corrupted.len()).step_by(100) {
        corrupted[i] = 0.0;
    }
    let corrupted_result = common::opus_compare::compare(&reference, &corrupted, 2);
    assert!(
        !corrupted_result.pass || corrupted_result.weighted_error > self_result.weighted_error,
        "corrupted signal must score worse than the self-comparison: {corrupted_result:?} vs {self_result:?}"
    );
}
