#![allow(clippy::unwrap_used)]

//! The output of the TwinVQ decoder, fed by the VQF reader, compared with the output of ffmpeg's
//! `twinvq` decoder, run live (the tests are skipped if `ffmpeg` is not installed).
//!
//! The fixtures `mono22_20.vqf`, `stereo22_48.vqf`, `stereo44_80.vqf` and `stereo44_96.vqf` are the
//! first 60000 bytes of `vqf/achterba.vqf`, `vqf/heaven.vqf`, `vqf/Target.vqf` and
//! `vqf/song_96k_high.vqf` from <https://samples.ffmpeg.org/vqf/>. The other bitrate modes have no
//! public samples; for them random streams are generated here (random payload, valid window
//! types), which exercise the decoder tables, whose values can all be reached by random indices.
//!
//! The comparison has a tolerance because the f32 output of the MDCT is not bit-for-bit portable.

use std::process::{Command, Stdio};

use symphonia_codec_twinvq::TwinVqDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_vqf::VqfReader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn open(data: Vec<u8>) -> VqfReader<'static> {
    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(data)),
        MediaSourceStreamOptions::default(),
    );
    VqfReader::try_new(mss, FormatOptions::default()).unwrap()
}

/// Decodes a VQF file to interleaved samples, returning them and the channel count.
fn decode(data: Vec<u8>) -> (Vec<f32>, usize) {
    let mut reader = open(data);
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };

    let mut decoder = TwinVqDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];
    let mut channels = 0;

    while let Some(packet) = reader.next_packet().unwrap() {
        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        channels = buf.spec().channels().count();
        for f in 0..buf.frames() {
            for c in 0..channels {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    (samples, channels)
}

/// Decodes a file with ffmpeg to interleaved f32 samples. `None` if ffmpeg is not available.
fn ffmpeg_decode(path: &std::path::Path) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    Some(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// Returns the SNR (dB) and the largest difference (16-bit LSBs) between the signals.
fn compare(ours: &[f32], reference: &[f32]) -> (f64, f64) {
    assert_eq!(ours.len(), reference.len(), "different number of samples");

    let mut sig = 0f64;
    let mut noise = 0f64;
    let mut max = 0f64;

    for (&a, &b) in ours.iter().zip(reference) {
        assert!(a.is_finite());
        sig += f64::from(b) * f64::from(b);
        let d = f64::from(a) - f64::from(b);
        noise += d * d;
        max = max.max(d.abs() * 32768.0);
    }

    (if noise == 0.0 { f64::INFINITY } else { 10.0 * (sig / noise).log10() }, max)
}

fn check_file(name: &str, data: Vec<u8>, min_snr: f64) {
    let path = std::env::temp_dir().join(format!("symphonia-twinvq-{name}.vqf"));
    std::fs::write(&path, &data).unwrap();
    let reference = ffmpeg_decode(&path);
    let _ = std::fs::remove_file(&path);

    let Some(reference) = reference
    else {
        eprintln!("ffmpeg not available, skipping {name}");
        return;
    };

    let (ours, channels) = decode(data);
    let (snr, max) = compare(&ours, &reference);
    eprintln!(
        "{name}: {} samples/channel, {channels} ch, SNR {snr:.1} dB, max diff {max:.3} LSB",
        ours.len() / channels
    );
    assert!(snr >= min_snr, "{name}: SNR {snr} dB");
}

#[test]
fn verify_real_fixtures_against_ffmpeg() {
    for name in ["mono22_20", "stereo22_48", "stereo44_80", "stereo44_96"] {
        let data = std::fs::read(format!("{FIXTURES}/{name}.vqf")).unwrap();
        check_file(name, data, 90.0);
    }
}

/// A xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u8
    }
}

/// Builds a VQF file with `frames` frames of random payload. The window type of every frame, the
/// first four bits, cycles through the valid values.
fn synthetic(rate_flag: u32, kbps: u32, channels: u32, frames: usize, seed: u64) -> Vec<u8> {
    let rate = match rate_flag {
        44 => 44100,
        22 => 22050,
        11 => 11025,
        n => n * 1000,
    };
    let size = match (rate_flag, kbps / channels) {
        (11, 8) | (8, 8) | (11, 10) | (22, 32) => 512,
        (16, 16) | (22, 20) | (22, 24) => 1024,
        _ => 2048,
    };
    let frame_bits = (kbps * 1000 * size / rate) as usize;

    let mut rng = Rng(seed);
    let mut payload: Vec<u8> =
        (0..(frames * frame_bits).div_ceil(8) + 2).map(|_| rng.next()).collect();

    for f in 0..frames {
        let wtype = (f % 9) as u8;
        for i in 0..4 {
            let bit = f * frame_bits + i;
            let value = (wtype >> (3 - i)) & 1;
            payload[bit / 8] = (payload[bit / 8] & !(0x80 >> (bit % 8))) | (value << (7 - bit % 8));
        }
    }

    let mut file = b"TWIN97012000".to_vec();
    file.extend_from_slice(&24u32.to_be_bytes());
    file.extend_from_slice(b"COMM");
    file.extend_from_slice(&16u32.to_be_bytes());
    for v in [channels - 1, kbps, rate_flag, 0] {
        file.extend_from_slice(&v.to_be_bytes());
    }
    file.extend_from_slice(b"DATA");
    file.extend_from_slice(&payload);
    file
}

/// The (rate flag, kbit/s per channel) pairs supported by the decoder.
const MODES: [(u32, u32); 9] =
    [(8, 8), (11, 8), (11, 10), (16, 16), (22, 20), (22, 24), (22, 32), (44, 40), (44, 48)];

#[test]
fn verify_synthetic_modes_against_ffmpeg() {
    for (i, &(rate, ibps)) in MODES.iter().enumerate() {
        for channels in [1, 2] {
            let data = synthetic(rate, ibps * channels, channels, 27, 0x9e37_79b9 + i as u64);
            check_file(&format!("synthetic{rate}_{ibps}x{channels}"), data, 90.0);
        }
    }
}

#[test]
fn verify_packet_layout_and_seek() {
    let data = std::fs::read(format!("{FIXTURES}/stereo22_48.vqf")).unwrap();
    let (all, channels) = decode(data.clone());

    let mut reader = open(data.clone());
    let frame_len = 1024u64;
    assert_eq!(
        reader.tracks()[0].num_frames,
        Some(all.len() as u64 / channels as u64 + 2 * frame_len)
    );

    // Seeking to a frame, then decoding, yields the samples of the full decode once the two
    // frames of start up of the decoder have passed.
    let mut decoder = TwinVqDecoder::try_new(
        match reader.tracks()[0].codec_params.as_ref().unwrap() {
            CodecParameters::Audio(p) => p,
            _ => panic!(),
        },
        &AudioDecoderOptions::default(),
    )
    .unwrap();

    let target = 20 * frame_len;
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(target as i64 + 5), track_id: 0 },
        )
        .unwrap();
    assert_eq!(seeked.actual_ts.get() as u64, target);

    let mut got = vec![];
    for n in 0..8 {
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts.get() as u64, target + n * frame_len);
        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!()
        };
        for f in 0..buf.frames() {
            for c in 0..channels {
                got.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    // The output of decoded frame n starts at sample (n - 2) * frame_len.
    let first = target as usize * channels;
    let reference = &all[first..];
    // The first two frames after the seek are not output; skip to the third frame, which
    // needs the transform history of the previous frames and so matches the full decode.
    let (snr, _) = compare(&got, &reference[..got.len()]);
    assert!(got.len() == 6 * frame_len as usize * channels, "{}", got.len());
    eprintln!("seek SNR {snr}");
    assert!(snr > 90.0, "SNR {snr}");

    // Seeking by time.
    reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Time { time: Time::try_new(1, 0).unwrap(), track_id: None },
        )
        .unwrap();
}

#[test]
fn verify_metadata() {
    let data = std::fs::read(format!("{FIXTURES}/mono22_20.vqf")).unwrap();
    let mut reader = open(data);
    let metadata = reader.metadata();
    let rev = metadata.current().unwrap();
    let title = rev
        .media
        .tags
        .iter()
        .find(|t| matches!(t.std, Some(symphonia_core::meta::StandardTag::TrackTitle(_))))
        .unwrap();
    assert_eq!(title.raw.value.to_string(), "Achterbahn");
}

#[test]
fn verify_malformed_files_do_not_panic() {
    let good = std::fs::read(format!("{FIXTURES}/mono22_20.vqf")).unwrap();
    let mut rng = Rng(77);

    for n in 0..300 {
        let mut data = good[..400 + n * 7].to_vec();
        // Corrupt some header and payload bytes.
        for _ in 0..(n % 5) {
            let at = usize::from(rng.next()) * 2 % data.len().min(300);
            data[at] = rng.next();
        }
        let mss = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        let Ok(mut reader) = VqfReader::try_new(mss, FormatOptions::default())
        else {
            continue;
        };
        let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
        else {
            continue;
        };
        let Ok(mut decoder) = TwinVqDecoder::try_new(&params, &AudioDecoderOptions::default())
        else {
            continue;
        };
        while let Ok(Some(packet)) = reader.next_packet() {
            let _ = decoder.decode(&packet);
        }
    }
}

#[test]
fn verify_probe_rejects_other_data() {
    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(vec![0u8; 100])),
        MediaSourceStreamOptions::default(),
    );
    assert!(VqfReader::try_new(mss, FormatOptions::default()).is_err());
}
