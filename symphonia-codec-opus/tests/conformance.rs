// RFC 8251 conformance harness for the full (SILK + Hybrid + CELT) Opus decoder, mirroring the
// official protocol in libopus `tests/run_vectors.sh` / `src/opus_demo.c` exactly:
//   - `opus_demo -d <rate> <channels> testvectorNN.bit tmp.out` decodes with a huge output
//     capacity per call (`max_frame_size = 2*48000`, i.e. effectively "as much as the packet
//     produces"); lost packets are decoded with `data = NULL` and `frame_size =
//     OPUS_GET_LAST_PACKET_DURATION` (mirrored here via `OpusDecoder::last_packet_duration`).
//   - `opus_compare [-s] testvectorNN[m].dec tmp.out` PASSES if either the `.dec` or `m.dec`
//     reference matches (both are full-rate stereo references; the mono comparison downmixes
//     via `.5*(L+R)` per `opus_compare.c`'s `main()`, matching `tests/silk_vectors.rs`).
//
// Run with:
//   OPUS_TESTVECTORS=/home/gianluca/M0Rf30/wt/ref/opus_newvectors \
//     cargo test -p symphonia-codec-opus --release --test conformance -- --ignored --nocapture

mod common;

use common::opus_demo::BitFile;
use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
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

/// C: `opus_compare.c` `main()`'s reference downmix (`x[xi]=.5*(x[2*xi]+x[2*xi+1])`), used when
/// comparing a mono-API decode against a stereo `.dec`/`m.dec` reference.
fn downmix_to_mono(stereo: &[f32]) -> Vec<f32> {
    stereo.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])).collect()
}

/// C: `FLOAT2INT16` (`opus_decode`'s int16-API wrapper around the float decode path, which is
/// what `opus_demo -d`/`opus_compare` actually compare against -- `.dec` references are raw
/// int16 PCM, NOT the ±1.0-range float API this crate's [`OpusDecoder::decode`] returns per the
/// assignment's float-API contract). Rescales and appends `samples` (±1.0 range) onto `out` in
/// int16 scale, matching libopus's `(short)FLOAT2INT(SATURATE(x*32768, 32767))`.
fn push_as_int16_scale(out: &mut Vec<f32>, samples: &[f32]) {
    out.extend(samples.iter().map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0)));
}

/// Walks a vector's packets and reports, per packet, the TOC-derived mode/bandwidth/frame
/// count/size and stereo flag — a debugging aid for wave-1 agents choosing vectors that isolate
/// specific coding modes.
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

/// Per-vector decode result.
struct VectorResult {
    /// Interleaved i16-range f32 PCM at 48 kHz, `channels` per frame.
    pcm: Vec<f32>,
    /// Count of packets (lost or not) whose decoder final range mismatched the encoder's
    /// recorded final range. Lost packets trivially match (both sides are 0 by convention: the
    /// encoder's `enc_final_range` for a genuinely dropped packet isn't meaningful, so those are
    /// excluded from the denominator entirely, matching `tests/silk_vectors.rs`'s convention).
    range_mismatches: usize,
    range_checked: usize,
}

/// Decodes an entire `.bit` vector at the given channel count through the full `OpusDecoder`,
/// following the exact `opus_demo -d`/`run_vectors.sh` protocol: a large fixed output capacity
/// per packet, PLC for lost packets sized from `last_packet_duration()`.
fn decode_vector(bit_path: &std::path::Path, channels: u8) -> VectorResult {
    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut decoder = OpusDecoder::try_new(SampleRate::Hz48000, channels).unwrap();
    let mut pcm = Vec::new();
    let mut range_mismatches = 0usize;
    let mut range_checked = 0usize;

    // C: `opus_demo.c`'s `max_frame_size = 48000*2` (2 s) -- a generous fixed capacity used for
    // every `opus_decode` call regardless of the actual packet duration.
    const MAX_FRAME_SIZE: usize = 96_000;

    let mut pkt_idx = 0usize;
    let debug = std::env::var("OPUS_DEBUG_MISMATCH").is_ok();
    let mut prev_mode_dbg = "none";
    for pkt in bitfile.iter() {
        pkt_idx += 1;
        let mut out = vec![0f32; MAX_FRAME_SIZE * channels as usize];
        if pkt.lost {
            let frame_size = decoder.last_packet_duration().max(48000 / 100);
            let n = decoder.decode(None, &mut out, frame_size).unwrap();
            push_as_int16_scale(&mut pcm, &out[..n * channels as usize]);
        }
        else {
            let n = decoder.decode(Some(&pkt.payload), &mut out, MAX_FRAME_SIZE).unwrap();
            push_as_int16_scale(&mut pcm, &out[..n * channels as usize]);
            range_checked += 1;
            let toc = packet::Toc::new(pkt.payload[0]);
            if decoder.final_range() != pkt.enc_final_range {
                range_mismatches += 1;
                if debug {
                    eprintln!(
                        "MISMATCH idx={pkt_idx} mode={:?} bw={:?} stereo={} fsz={} prev={prev_mode_dbg}",
                        toc.mode(), toc.bandwidth(), toc.stereo(), toc.samples_per_frame(48000)
                    );
                }
            }
            prev_mode_dbg = match toc.mode() {
                OpusMode::SilkOnly => "silk",
                OpusMode::Hybrid => "hybrid",
                OpusMode::CeltOnly => "celt",
            };
        }
    }

    VectorResult { pcm, range_mismatches, range_checked }
}

macro_rules! conformance_test {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            let Some(dir) = testvectors_dir()
            else {
                eprintln!("OPUS_TESTVECTORS not set; skipping");
                return;
            };
            let bit_path = dir.join(format!("testvector{:02}.bit", $index));

            // Mono: opus_compare (no -s); reference is ALWAYS full-rate stereo and gets
            // downmixed by opus_compare.c's main() before comparing.
            let mono = decode_vector(&bit_path, 1);
            assert_eq!(
                mono.range_mismatches, 0,
                "final range mismatches (mono): {}/{}",
                mono.range_mismatches, mono.range_checked
            );
            let reference = read_dec_as_f32(&dir.join(format!("testvector{:02}.dec", $index)), 2);
            let reference_m = read_dec_as_f32(&dir.join(format!("testvector{:02}m.dec", $index)), 2);
            let ref_mono = downmix_to_mono(&reference);
            let ref_mono_m = downmix_to_mono(&reference_m);
            let r1 = common::opus_compare::compare(&ref_mono, &mono.pcm, 1);
            let r2 = common::opus_compare::compare(&ref_mono_m, &mono.pcm, 1);
            assert!(r1.pass || r2.pass, "opus_compare FAILS (mono) vs both refs: {r1:?} / {r2:?}");

            // Stereo: opus_compare -s.
            let stereo = decode_vector(&bit_path, 2);
            assert_eq!(
                stereo.range_mismatches, 0,
                "final range mismatches (stereo): {}/{}",
                stereo.range_mismatches, stereo.range_checked
            );
            let r1 = common::opus_compare::compare(&reference, &stereo.pcm, 2);
            let r2 = common::opus_compare::compare(&reference_m, &stereo.pcm, 2);
            assert!(r1.pass || r2.pass, "opus_compare FAILS (stereo) vs both refs: {r1:?} / {r2:?}");
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

/// Validates the [`common::opus_compare`] port independent of the decoder: a `.dec` file must
/// compare equal to itself, and a mildly corrupted copy must fail.
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

/// Release-mode decode-time measurement for a CELT-heavy stereo vector (step 5 of the
/// assignment).
#[test]
#[ignore = "run explicitly with --release --ignored to measure x-realtime"]
fn vector11_perf() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping");
        return;
    };
    let bit_path = dir.join("testvector11.bit");
    let data = std::fs::read(&bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let start = std::time::Instant::now();
    let result = decode_vector(&bit_path, 2);
    let elapsed = start.elapsed();

    let audio_seconds = (result.pcm.len() as f64 / 2.0) / 48000.0;
    println!(
        "vector11: decoded {} packets ({:.3}s audio) in {:?} => {:.1}x realtime",
        bitfile.len(),
        audio_seconds,
        elapsed,
        audio_seconds / elapsed.as_secs_f64()
    );
}
