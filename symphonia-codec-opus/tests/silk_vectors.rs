// RFC 8251 conformance harness for the SILK-only decode path, mirroring the SILK branch of
// libopus `src/opus_decoder.c`'s `opus_decode_native`/`opus_decode_frame` (TOC-driven mode /
// bandwidth / frame-size dispatch, `silk_Decode` per internal 20 ms sub-frame, PLC on loss) --
// independent of (and not requiring) the wave-2 `OpusDecoder`.
//
// Run with:
//   OPUS_TESTVECTORS=/home/gianluca/M0Rf30/wt/ref/opus_newvectors \
//     cargo test -p symphonia-codec-opus --release --test silk_vectors -- --ignored --nocapture

#[path = "common/opus_demo.rs"]
mod opus_demo;
#[path = "common/opus_compare.rs"]
mod opus_compare;

use opus_demo::BitFile;
use symphonia_codec_opus::packet::{self, Bandwidth, OpusMode};
use symphonia_codec_opus::range::RangeDecoder;
use symphonia_codec_opus::silk::{DecControl, DecodeFlag, SilkDecoder};

fn testvectors_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("OPUS_TESTVECTORS").map(std::path::PathBuf::from)
}

fn read_dec_as_f32(path: &std::path::Path, channels: usize) -> Vec<f32> {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    assert_eq!(raw.len() % 2, 0);
    let samples: Vec<f32> =
        raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32).collect();
    assert_eq!(samples.len() % channels, 0);
    samples
}

/// Per-vector decode result.
struct VectorResult {
    /// Interleaved i16-range f32 PCM at 48 kHz, `channels` per frame.
    pcm: Vec<f32>,
    /// Count of non-lost, SILK-only packets whose decoder final range mismatched the encoder's.
    silk_range_mismatches: usize,
    /// Count of non-lost, SILK-only packets checked at all (denominator for the above).
    silk_packets_checked: usize,
    /// Count of non-lost packets that were NOT SilkOnly mode (skipped; e.g. Hybrid/CELT frames
    /// in mixed-mode vectors like testvector12).
    non_silk_packets: usize,
}

/// Remembered per-channel decode parameters, carried across a lost packet (which has no TOC) so
/// PLC can run with the same framing as the last successfully parsed packet.
#[derive(Clone, Copy)]
struct LastConfig {
    n_channels_internal: i32,
    internal_sample_rate: i32,
    payload_size_ms: i32,
}

fn decode_silk_vector(bit_path: &std::path::Path, channels: u8) -> VectorResult {
    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut decoder = SilkDecoder::new();
    let mut pcm = Vec::new();
    let mut silk_range_mismatches = 0usize;
    let mut silk_packets_checked = 0usize;
    let mut non_silk_packets = 0usize;
    let mut last_config: Option<LastConfig> = None;
    let mut total_n_sum = 0usize;
    let mut total_calls = 0usize;
    let mut zero_n_calls = 0usize;

    for pkt in bitfile.iter() {
        if pkt.lost {
            // Packet Loss Concealment: reuse the last known framing (if any).
            let Some(cfg) = last_config
            else {
                continue;
            };
            let n_frames = (cfg.payload_size_ms / 20).max(1) as usize;
            let mut ctl = DecControl {
                n_channels_api: channels as i32,
                n_channels_internal: cfg.n_channels_internal,
                api_sample_rate: 48000,
                internal_sample_rate: cfg.internal_sample_rate,
                payload_size_ms: cfg.payload_size_ms,
                prev_pitch_lag: 0,
            };
            // A lost packet has no payload to build a range decoder from; libopus's PLC path
            // never touches the range decoder at all, so we don't construct one here either.
            // We still need *a* `RangeDecoder` value to satisfy the API; PLC decode paths never
            // read from it (verified: `decode_frame`'s `PacketLost` branch never calls `rd`).
            let empty: [u8; 1] = [0];
            let mut rd = RangeDecoder::new(&empty);
            for f in 0..n_frames {
                let mut out = vec![0i16; 48 * 60 * channels as usize];
                let mut n = 0usize;
                decoder
                    .decode(&mut ctl, DecodeFlag::PacketLost, f == 0, &mut rd, &mut out, &mut n)
                    .unwrap();
                for i in 0..n {
                    for c in 0..channels as usize {
                        pcm.push(out[c + channels as usize * i] as f32);
                    }
                }
            }
            continue;
        }

        let Ok(parsed) = packet::parse(&pkt.payload)
        else {
            continue;
        };

        for frame in &parsed.frames {
            let payload = &pkt.payload[frame.offset..frame.offset + frame.len];
            if parsed.toc.mode() != OpusMode::SilkOnly {
                non_silk_packets += 1;
                continue;
            }

            let n_channels_internal = if parsed.toc.stereo() { 2 } else { 1 };
            let internal_sample_rate = match parsed.toc.bandwidth() {
                Bandwidth::Narrowband => 8000,
                Bandwidth::Mediumband => 12000,
                _ => 16000,
            };
            let payload_size_ms = (parsed.toc.samples_per_frame(48000) / 48) as i32;

            last_config =
                Some(LastConfig { n_channels_internal, internal_sample_rate, payload_size_ms });

            let mut ctl = DecControl {
                n_channels_api: channels as i32,
                n_channels_internal,
                api_sample_rate: 48000,
                internal_sample_rate,
                payload_size_ms,
                prev_pitch_lag: 0,
            };

            let mut rd = RangeDecoder::new(payload);
            let n_frames = (payload_size_ms / 20).max(1) as usize;
            for f in 0..n_frames {
                let mut out = vec![0i16; 48 * 60 * channels as usize];
                let mut n = 0usize;
                decoder.decode(&mut ctl, DecodeFlag::Normal, f == 0, &mut rd, &mut out, &mut n).unwrap();
                total_n_sum += n;
                total_calls += 1;
                if n == 0 {
                    zero_n_calls += 1;
                }
                for i in 0..n {
                    for c in 0..channels as usize {
                        pcm.push(out[c + channels as usize * i] as f32);
                    }
                }
            }

            silk_packets_checked += 1;
            if rd.range() != pkt.enc_final_range {
                silk_range_mismatches += 1;
            }
        }
    }

    if std::env::var("SILK_DEBUG").is_ok() {
        eprintln!(
            "channels={channels} total_calls={total_calls} total_n_sum={total_n_sum} zero_n_calls={zero_n_calls} pcm.len()={}",
            pcm.len()
        );
    }
    VectorResult { pcm, silk_range_mismatches, silk_packets_checked, non_silk_packets }
}

macro_rules! silk_vector_test {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            let Some(dir) = testvectors_dir()
            else {
                eprintln!("OPUS_TESTVECTORS not set; skipping");
                return;
            };
            let bit_path = dir.join(format!("testvector{:02}.bit", $index));

            let stereo = decode_silk_vector(&bit_path, 2);
            assert_eq!(
                stereo.silk_range_mismatches, 0,
                "final range mismatches (stereo): {}/{} SILK packets",
                stereo.silk_range_mismatches, stereo.silk_packets_checked
            );
            let reference = read_dec_as_f32(&dir.join(format!("testvector{:02}.dec", $index)), 2);
            if stereo.non_silk_packets == 0 {
                let result = opus_compare::compare(&reference, &stereo.pcm, 2);
                assert!(result.pass, "opus_compare FAILS (stereo): {result:?}");
            }

            let mono = decode_silk_vector(&bit_path, 1);
            assert_eq!(
                mono.silk_range_mismatches, 0,
                "final range mismatches (mono): {}/{} SILK packets",
                mono.silk_range_mismatches, mono.silk_packets_checked
            );
            // NOTE: despite the name, `testvectorNNm.dec` is NOT raw mono PCM -- it is always
            // exactly the same byte size as `testvectorNN.dec` (verified empirically across all
            // 12 vectors). It stores the channels=1-API decode result with each sample
            // duplicated to stereo (L=R=mono sample) for the same 48 kHz stereo `.dec` layout,
            // so we compare it the same way (`channels=2`) against a locally-duplicated version
            // of our mono decode.
            let reference_m = read_dec_as_f32(&dir.join(format!("testvector{:02}m.dec", $index)), 2);
            if mono.non_silk_packets == 0 {
                let mono_dup: Vec<f32> = mono.pcm.iter().flat_map(|&s| [s, s]).collect();
                let result_m = opus_compare::compare(&reference_m, &mono_dup, 2);
                assert!(result_m.pass, "opus_compare FAILS (mono, duplicated to stereo): {result_m:?}");
            }
        }
    };
}

silk_vector_test!(vector02, 2);
silk_vector_test!(vector03, 3);
silk_vector_test!(vector04, 4);

#[test]
fn vector04_mono_vs_stereo_left_consistency() {
    let Some(dir) = testvectors_dir()
    else {
        return;
    };
    let bit_path = dir.join("testvector04.bit");
    let stereo = decode_silk_vector(&bit_path, 2);
    let mono = decode_silk_vector(&bit_path, 1);
    let left: Vec<f32> = stereo.pcm.iter().step_by(2).copied().collect();
    println!("left.len()={} mono.len()={}", left.len(), mono.pcm.len());
    let n = left.len().min(mono.pcm.len());
    let mut first_diff = None;
    let mut max_diff = 0f32;
    for i in 0..n {
        let d = (left[i] - mono.pcm[i]).abs();
        if d > max_diff {
            max_diff = d;
        }
        if d > 0.5 && first_diff.is_none() {
            first_diff = Some(i);
        }
    }
    println!("first_diff={:?} max_diff={}", first_diff, max_diff);
}

/// Vector 12 ("mostly SILK+Hybrid mono", per the assignment's facts) mixes coding modes; this
/// only reports the SILK-only-packet final-range match rate (no `opus_compare`, since our
/// harness doesn't drive CELT/Hybrid).
#[test]
fn vector12_silk_only_range_match_rate() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping");
        return;
    };
    let bit_path = dir.join("testvector12.bit");
    let result = decode_silk_vector(&bit_path, 1);
    println!(
        "vector12 SILK-only packets: {} checked, {} range mismatches, {} non-SILK packets skipped",
        result.silk_packets_checked, result.silk_range_mismatches, result.non_silk_packets
    );
}

/// Release-mode decode-time measurement for vector 04 (per the assignment's step 5).
#[test]
#[ignore = "run explicitly with --release --ignored to measure x-realtime"]
fn vector04_perf() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping");
        return;
    };
    let bit_path = dir.join("testvector04.bit");
    let data = std::fs::read(&bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let start = std::time::Instant::now();
    let result = decode_silk_vector(&bit_path, 2);
    let elapsed = start.elapsed();

    let audio_seconds = (result.pcm.len() as f64 / 2.0) / 48000.0;
    println!(
        "vector04: decoded {} packets ({:.3}s audio) in {:?} => {:.1}x realtime",
        bitfile.len(),
        audio_seconds,
        elapsed,
        audio_seconds / elapsed.as_secs_f64()
    );
}
