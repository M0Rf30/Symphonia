// Regression tests that decode real DSD sample files. They run only when the `RMPD_SAMPLES`
// environment variable points at the sample directory (containing `dsd/`).
#![allow(clippy::unwrap_used)]

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
/// unfused, fixed-order floating point in the FIR stages, so the output is identical in every
/// build configuration; any change to these hashes is a change of decoder output. (The taps of
/// the FIR stages are computed with the `sin` and `cos` of the platform, so a platform with a
/// different libm might differ in the last bit of some samples.)
/// (The CIC stage itself was verified bit-identical to an integrator/comb implementation on all
/// sample files.) Each case is a whole number of frames short of the frames of the packets by the
/// delay of the filters, which is dropped from the start of the stream.
#[test]
fn test_pinned_pcm_output_hashes() {
    let cases: &[(&str, u32, usize, u64, usize)] = &[
        // (file, rate, packets, hash, samples)
        ("dsd64_dsf.dsf", 44100, 200, 0x5ff94f4fb733dd25, 204714),
        ("dsd64_dff.dff", 352800, 200, 0x87e024623e1b7268, 819122),
        ("dsd128_dsf.dsf", 44100, 200, 0x4ecd33f30c391e5f, 102314),
        ("dsd128_dff.dff", 88200, 200, 0x50f9d93082880147, 102314),
        ("dsd256_dsf.dsf", 176400, 200, 0x12f6daf0d2a3c503, 204714),
        ("dsd256_dff.dff", 352800, 200, 0x45b187c259c5d958, 204714),
        ("dsd64_dsf_6ch.dsf", 88200, 200, 0xf75b770c47d7b2dc, 1228542),
    ];

    for &(name, rate, packets, hash, samples) in cases {
        if let Some((got_hash, got_samples)) = decode_hash(name, rate, packets) {
            assert_eq!(got_samples, samples, "{name} @ {rate}: sample count");
            assert_eq!(got_hash, hash, "{name} @ {rate}: hash {got_hash:016x}");
        }
    }
}

/// A DST compressed DSDIFF file (FFmpeg FATE `dst/dst-64fs44-2ch.dff`: 10 frames of 2 channels
/// of DSD64) decodes with the frame CRC and arithmetic code checks enabled to exactly the DSD that
/// the DST reference decoder (the MPEG-4 reference software, `dstcnv`) produces.
#[test]
fn test_dst_decodes_to_reference_dsd() {
    let Some(path) = std::env::var_os("RMPD_SAMPLES")
        .map(|dir| PathBuf::from(dir).join("dsd").join("dst-64fs44-2ch.dff"))
        .filter(|path| path.exists())
    else {
        eprintln!("skipping: dst-64fs44-2ch.dff not found");
        return;
    };

    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    let mut reader =
        symphonia_format_dsd::DffReader::try_new(mss, FormatOptions::default()).unwrap();
    let params = match reader.tracks()[0].codec_params.as_ref() {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("expected an audio track"),
    };
    assert_eq!(params.codec, symphonia_codec_dsd::CODEC_ID_DST);

    let options = AudioDecoderOptions::default().verify(true);
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // FNV-1a of the DSD, interleaved by channel.
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut bytes = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        let GenericAudioBufferRef::U8(buf) = decoder.decode_ref(&packet.as_packet_ref()).unwrap()
        else {
            panic!("expected U8 output");
        };
        let planes = [buf.plane(0).unwrap(), buf.plane(1).unwrap()];
        for i in 0..buf.frames() {
            for plane in planes {
                hash = (hash ^ u64::from(plane[i])).wrapping_mul(0x100000001b3);
                bytes += 1;
            }
        }
    }

    assert_eq!(bytes, 94080);
    assert_eq!(hash, 0x21312de594ef8567);
    assert_eq!(decoder.finalize().verify_ok, Some(true));
}
