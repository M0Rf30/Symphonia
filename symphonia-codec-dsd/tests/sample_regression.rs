// Regression tests that decode real DSD sample files. They run only when the `RMPD_SAMPLES`
// environment variable points at the sample directory (containing `dsd/`).

use std::fs::File;
use std::path::PathBuf;

use symphonia_codec_dsd::DsdDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Decode up to `max_packets` packets of a sample to PCM at `pcm_rate`; returns the FNV-1a hash
/// of the little-endian `f32` output (plane by plane, packet by packet) and the sample count.
fn decode_hash(name: &str, pcm_rate: u32, max_packets: usize) -> Option<(u64, usize)> {
    let path = PathBuf::from(std::env::var_os("RMPD_SAMPLES")?).join("dsd").join(name);
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        return None;
    }

    let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
    let mut reader: Box<dyn FormatReader> = if name.ends_with(".dff") {
        Box::new(symphonia_format_dsd::DffReader::try_new(mss, FormatOptions::default()).unwrap())
    }
    else {
        Box::new(symphonia_format_dsd::DsfReader::try_new(mss, FormatOptions::default()).unwrap())
    };

    let (track_id, mut params) = reader
        .tracks()
        .iter()
        .find_map(|t| match t.codec_params.as_ref() {
            Some(CodecParameters::Audio(a)) => Some((t.id, a.clone())),
            _ => None,
        })
        .unwrap();
    params.extra_data = Some(pcm_rate.to_le_bytes().to_vec().into_boxed_slice());
    let mut decoder = DsdDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut hash: u64 = 0xcbf29ce484222325;
    let mut total = 0;

    for _ in 0..max_packets {
        let Some(packet) = reader.next_packet().unwrap()
        else {
            break;
        };
        if packet.track_id != track_id {
            continue;
        }
        let GenericAudioBufferRef::F32(buf) = decoder.decode_ref(&packet.as_packet_ref()).unwrap()
        else {
            panic!("expected F32 output");
        };
        for ch in 0..buf.spec().channels().count() {
            for &s in buf.plane(ch).unwrap() {
                assert!(s.is_finite());
                for b in s.to_bits().to_le_bytes() {
                    hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
                total += 1;
            }
        }
    }

    Some((hash, total))
}

/// Pinned output hashes. The decimation chain uses exact integer arithmetic in the CIC stage and
/// unfused, fixed-order floating point in the FIR stage, so the output is identical on every
/// target and build configuration; any change to these hashes is a change of decoder output.
/// (The CIC stage itself was verified bit-identical to the former integrator/comb
/// implementation on all sample files.)
#[test]
fn test_pinned_pcm_output_hashes() {
    let cases: &[(&str, u32, usize, u64, usize)] = &[
        // (file, rate, packets, hash, samples)
        ("dsd64_dsf.dsf", 44100, 200, 0x8d5fd572354a1cc3, 204800),
        ("dsd64_dff.dff", 352800, 200, 0x0152fb40068f7bef, 819200),
        ("dsd128_dsf.dsf", 44100, 200, 0x7fd76d730f30c06a, 102400),
        ("dsd128_dff.dff", 88200, 200, 0xa16ba6ed6fdb2937, 102400),
        ("dsd256_dsf.dsf", 176400, 200, 0x8eb01c6d56e2f271, 204800),
        ("dsd256_dff.dff", 352800, 200, 0x3acb551cf804ce24, 204800),
        ("dsd64_dsf_6ch.dsf", 88200, 200, 0x5cf57176d5f1e098, 1228800),
    ];

    for &(name, rate, packets, hash, samples) in cases {
        if let Some((got_hash, got_samples)) = decode_hash(name, rate, packets) {
            assert_eq!(got_samples, samples, "{name} @ {rate}: sample count");
            assert_eq!(got_hash, hash, "{name} @ {rate}: hash {got_hash:016x}");
        }
    }
}
