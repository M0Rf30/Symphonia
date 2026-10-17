#![allow(clippy::unwrap_used)]

//! Gapless playback with the mp3PRO decoder.
//!
//! The SBR output lags the core by 962 samples at the output rate (481 core samples). With
//! gapless playback enabled, the decoder removes the delay and padding of the stream (counted in
//! core samples) and this lag, so the audio it outputs is the audio between the delay and
//! padding, aligned with the core. Plain streams have no SBR output and no lag: they are trimmed
//! like [`MpaDecoder`] does.

use std::fs::File;
use std::io::Cursor;

use symphonia_bundle_mp3::{Mp3ProDecoder, MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::Duration;
use symphonia_testutil::sample_path;

/// An MPEG-2 22.05 kHz mono LAME stream (delay 1105, padding 542, 11025 frames).
const MONO_22K: &[u8] = include_bytes!("fixtures/mp3_mpeg2_mono_22k.mp3");

/// The delay of the SBR output, in samples at the output rate.
const SBR_DELAY: usize = 962;

fn decode_all(decoder: &mut dyn AudioDecoder, packets: &[Packet], expected_rate: u32) -> Vec<f32> {
    let mut samples = Vec::new();

    for packet in packets {
        let decoded = decoder.decode(packet).unwrap();
        assert_eq!(decoded.spec().rate(), expected_rate);

        let mut buf = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        samples.extend_from_slice(&buf);
    }

    samples
}

fn read_packets(mss: MediaSourceStream<'static>) -> (Vec<Packet>, AudioCodecParameters) {
    let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();
    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone();
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push(packet);
    }

    (packets, params)
}

#[test]
fn verify_plain_stream_is_trimmed_like_the_iso_decoder() {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(MONO_22K.to_vec())), Default::default());
    let (packets, params) = read_packets(mss);

    let opts = AudioDecoderOptions::default().gapless(true);
    let plain = decode_all(&mut MpaDecoder::try_new(&params, &opts).unwrap(), &packets, 22050);
    let pro = decode_all(&mut Mp3ProDecoder::try_new(&params, &opts).unwrap(), &packets, 22050);

    // The delay and the padding are removed.
    assert_eq!(pro.len(), 11025);
    assert_eq!(pro, plain);
}

#[test]
fn verify_gapless_removes_delay_padding_and_sbr_delay() {
    // A real mp3PRO stream has no encoder delay or padding. Give it the delay and padding of a
    // LAME stream.
    const DELAY: usize = 1105;
    const PADDING: usize = 542;

    let Some(path) = sample_path("mp3pro/solaar.mp3")
    else {
        return;
    };

    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    let (mut packets, params) = read_packets(mss);
    packets.truncate(100);

    let full = decode_all(
        &mut Mp3ProDecoder::try_new(&params, &AudioDecoderOptions::default().gapless(false))
            .unwrap(),
        &packets,
        44100,
    );

    // The trims are in core samples.
    let mut left = DELAY;
    for packet in &mut packets {
        let trim = left.min(576);
        packet.trim_start = Duration::from(trim as u32);
        left -= trim;
    }
    let last = packets.last_mut().unwrap();
    last.trim_end = Duration::from(PADDING as u32);

    let trimmed = decode_all(
        &mut Mp3ProDecoder::try_new(&params, &AudioDecoderOptions::default().gapless(true))
            .unwrap(),
        &packets,
        44100,
    );

    // The samples are interleaved stereo. The padding is longer than the SBR delay, so all of the
    // audio between the delay and the padding is decoded.
    let frames = packets.len() * 576 - DELAY - PADDING;
    let start = 2 * (2 * DELAY + SBR_DELAY);
    assert_eq!(trimmed.len(), 2 * 2 * frames);
    assert_eq!(trimmed, full[start..start + 2 * 2 * frames]);
}
