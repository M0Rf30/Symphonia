#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia::core::audio::{Channels, Position};
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_COOK;
use symphonia::default::codecs::CookDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the channel count, the second the length of the extra data (the
    // RealAudio Cook header); the rest is a sequence of length-prefixed packets.
    let [select, extra_len, rest @ ..] = data
    else {
        return;
    };

    let (extra, packets) = rest.split_at(usize::from(*extra_len).min(rest.len()));

    let channels = match select % 4 {
        0 => CHANNEL_LAYOUT_MONO,
        1 => CHANNEL_LAYOUT_STEREO,
        2 => Channels::Positioned(
            Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER,
        ),
        _ => Channels::Positioned(
            Position::from_wave_channel_count(6).unwrap_or(Position::FRONT_LEFT),
        ),
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_COOK)
        .with_sample_rate(44100)
        .with_channels(channels)
        .with_extra_data(extra.to_vec().into_boxed_slice());

    fuzz_audio_decoder_packets::<CookDecoder>(&codec_params, packets);
});
