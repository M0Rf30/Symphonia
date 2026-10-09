//! Golden hashes of the decoded output of streams with SBR (HE-AAC v1/v2, AAC-ELD with SBR) and of
//! the LC core. The hash is FNV-1a over the bit patterns of the interleaved f32 samples, so it pins
//! the output of the decoder bit for bit: optimisations of the SBR filter banks, the envelope
//! decoding, etc. must not change it.
//!
//! The in-tree fixtures are always checked. The generated format-coverage samples are checked if
//! `RMPD_SAMPLES` points at them. Set `SBR_GOLDEN_PRINT` to print the table instead of comparing.
//!
//! The `sbr-fft-qmf` feature computes the QMF banks with an FFT, which differs from the pinned output
//! by rounding errors, so the hashes are only checked without it.

#![cfg(not(feature = "sbr-fft-qmf"))]

use symphonia_codec_aac::{AacDecoder, AdtsReader, LoasReader};
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::formats::{FormatReader, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_format_isomp4::IsoMp4Reader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn add(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn hash_buffer(h: &mut Fnv, buf: &GenericAudioBufferRef<'_>) {
    let GenericAudioBufferRef::F32(buf) = buf
    else {
        panic!("expected f32 samples");
    };

    let n_ch = buf.spec().channels().count();
    h.add(buf.frames() as u32);
    h.add(n_ch as u32);
    h.add(buf.spec().rate());

    for f in 0..buf.frames() {
        for c in 0..n_ch {
            h.add(buf.plane(c).unwrap()[f].to_bits());
        }
    }
}

fn open(path: &std::path::Path, kind: &str) -> Option<Box<dyn FormatReader>> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    Some(match kind {
        "adts" => AdtsReader::try_probe_new(mss, Default::default()).unwrap(),
        "loas" => LoasReader::try_probe_new(mss, Default::default()).unwrap(),
        _ => IsoMp4Reader::try_probe_new(mss, Default::default()).unwrap(),
    })
}

fn hash_stream(path: &std::path::Path, kind: &str) -> Option<u64> {
    let mut reader = open(path, kind)?;

    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };

    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut h = Fnv::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id == id {
            hash_buffer(&mut h, &decoder.decode(&packet).unwrap());
        }
    }

    Some(h.0)
}

fn hash_bin(path: &std::path::Path, frame_len: usize) -> u64 {
    let data = std::fs::read(path).unwrap();
    let mut records = vec![];
    let mut pos = 0;

    while pos < data.len() {
        let len = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
        records.push(data[pos + 2..pos + 2 + len].to_vec());
        pos += 2 + len;
    }

    let asc_bytes = records.remove(0);
    let asc = AudioSpecificConfig::read(&asc_bytes).unwrap();
    let mut params = AudioCodecParameters::new();

    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_extra_data(asc_bytes.as_slice().into());

    if let Some(channels) = asc.output_channels() {
        params.with_channels(channels);
    }

    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut h = Fnv::new();

    for (i, au) in records.into_iter().enumerate() {
        let packet = Packet::new(
            0,
            Timestamp::new((i * frame_len) as i64),
            Duration::new(frame_len as u64),
            au.into_boxed_slice(),
        );
        hash_buffer(&mut h, &decoder.decode(&packet).unwrap());
    }

    h.0
}

/// (label, kind, file, golden hash). A kind of `bin` is a fixture of access units.
const FIXTURE_CASES: &[(&str, &str, u64)] = &[
    ("he_aac_v1_mono.aac", "adts", 0xac21fb661844d3f8),
    ("he_aac_v1_stereo.aac", "adts", 0xa7fd2ca7eeceff4c),
    ("he_aac_v2.aac", "adts", 0x902e128b72a6b6fa),
    ("aac_lc_22k_mono.aac", "adts", 0xf0625fcda47da9b5),
    ("aac_eld_sbr_480_mono.bin", "bin480", 0xfde3ef2cee24301e),
    ("aac_eld_sbr_512_dual_stereo.bin", "bin512", 0xa02a510eaa36b0bb),
    ("aac_eld_sbr_512_mono.bin", "bin512", 0xbf02fb6a6b4a2e16),
    ("aac_eld_sbr_512_mono_22k.bin", "bin512", 0x97d573c16d85c765),
    ("aac_eld_480_mono.bin", "bin480", 0x97af8904025a5aba),
    ("aac_ld_512_mono.bin", "bin512", 0xb07fcdceb41d7081),
];

const SAMPLE_CASES: &[(&str, &str, u64)] = &[
    ("heaac_v1_fdk_adts.aac", "adts", 0xb0fda23d1ada3601),
    ("heaac_v2_fdk_adts.aac", "adts", 0xfffca6a94bd8482c),
    ("heaac_v1_fdk_m4a.m4a", "mp4", 0xc2ce160a433304f6),
    ("heaac_v1_fdk_mono.m4a", "mp4", 0x9eb47d4a1875f935),
    ("heaac_v1_fdk_5_1.m4a", "mp4", 0x69849d87c8a98587),
    ("heaac_v1_fdk_downsampled_sbr.m4a", "mp4", 0xcddc68950b6dcf3e),
    ("heaac_v2_fdk_m4a.m4a", "mp4", 0xce95b55060d7d7f8),
    ("heaac_nero_sample.mp4", "mp4", 0x69cb203791622fa0),
    ("aac_eld_sbr_fdk.m4a", "mp4", 0x2081fb9411d892b6),
    ("aac_eld_sbr_dual_fdk.m4a", "mp4", 0x8ff1cae748a8bfc8),
    ("aac_eld_sbr_480_latm.aac", "loas", 0x1708ec866d46ee44),
    ("aac_lc_fdk_48k.m4a", "mp4", 0x9b78e17eee82f309),
];

fn run(cases: &[(&str, &str, u64)], dir: &std::path::Path) {
    let print = std::env::var_os("SBR_GOLDEN_PRINT").is_some();

    for &(name, kind, golden) in cases {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }

        let got = match kind {
            "bin480" => hash_bin(&path, 480),
            "bin512" => hash_bin(&path, 512),
            _ => hash_stream(&path, kind).unwrap(),
        };

        if print {
            println!("    (\"{name}\", \"{kind}\", 0x{got:016x}),");
        }
        else {
            assert_eq!(got, golden, "{name}: decoded output changed");
        }
    }
}

#[test]
fn fixtures_decode_to_the_golden_output() {
    run(FIXTURE_CASES, std::path::Path::new(FIXTURES));
}

#[test]
fn samples_decode_to_the_golden_output() {
    let Some(dir) = std::env::var_os("RMPD_SAMPLES")
    else {
        return;
    };
    run(SAMPLE_CASES, &std::path::Path::new(&dir).join("aac"));
}
