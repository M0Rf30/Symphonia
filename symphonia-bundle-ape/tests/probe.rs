// Tests for the APE `FormatReader`: probe marker detection, header parsing, and duration
// derivation, against a synthetic in-memory APE header (no compressed audio frame data).
//
// A full end-to-end decode test would require a real, non-trivial `.ape` fixture that isn't
// available in this repository; `ape_decoder::FrameDecoder` itself is exhaustively tested
// upstream (see the `ape-decoder` crate).

use std::io::Cursor;

use symphonia_bundle_ape::ApeReader;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MONKEYS_AUDIO;
use symphonia_core::formats::probe::{Score, Scoreable};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions, ScopedStream};

/// A minimal, valid current-format (version 3990) APE descriptor + header, with no seek table,
/// no WAV header data, and no frame data (`total_frames = 1`, `blocks_per_frame =
/// final_frame_blocks = sample_rate = 44100`, 16-bit stereo). This is enough for
/// `ape_decoder::format::parse` to succeed and derive a 1 second duration.
const SYNTHETIC_APE_HEADER: [u8; 76] = [
    0x4d, 0x41, 0x43, 0x20, 0x96, 0x0f, 0x00, 0x00, 0x34, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xd0, 0x07, 0x00, 0x00, 0x44, 0xac, 0x00, 0x00, 0x44, 0xac, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x10, 0x00, 0x02, 0x00, 0x44, 0xac, 0x00, 0x00,
];

fn create_stream(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default())
}

#[test]
fn scores_mac_marker_as_supported() {
    let mut mss = create_stream(SYNTHETIC_APE_HEADER.to_vec());
    let scoped = ScopedStream::new(&mut mss, 4);

    assert!(matches!(ApeReader::score(scoped), Ok(Score::Supported(_))));
}

#[test]
fn scores_non_matching_stream_as_unsupported() {
    let mut mss = create_stream(vec![0u8; 16]);
    let scoped = ScopedStream::new(&mut mss, 4);

    assert!(matches!(ApeReader::score(scoped), Ok(Score::Unsupported)));
}

#[test]
fn parses_header_and_derives_duration() {
    let mss = create_stream(SYNTHETIC_APE_HEADER.to_vec());
    let reader = ApeReader::try_new(mss, FormatOptions::default()).expect("valid synthetic header");

    let track = reader.tracks().first().expect("one track");
    let CodecParameters::Audio(params) =
        track.codec_params.as_ref().expect("codec params present")
    else {
        panic!("expected audio codec parameters");
    };

    assert_eq!(params.codec, CODEC_ID_MONKEYS_AUDIO);
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.bits_per_sample, Some(16));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));

    // total_frames=1, blocks_per_frame=final_frame_blocks=44100 => 44100 total blocks/samples.
    assert_eq!(track.num_frames, Some(44_100));

    let time_base = track.time_base.expect("time base derived from sample rate");
    let duration_ticks = track
        .duration
        .unwrap_or_else(|| symphonia_core::units::Duration::new(track.num_frames.expect("num_frames set")));
    let duration = time_base.calc_duration(duration_ticks).expect("duration fits");

    // 44100 samples at 44100 Hz is exactly 1 second.
    assert_eq!(duration.as_secs(), 1);
    assert_eq!(duration.as_nanos(), 1_000_000_000);
}

#[test]
fn rejects_non_ape_stream() {
    let mss = create_stream(vec![0u8; 76]);
    assert!(ApeReader::try_new(mss, FormatOptions::default()).is_err());
}
