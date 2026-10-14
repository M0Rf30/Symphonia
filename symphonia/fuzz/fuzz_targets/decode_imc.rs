#![no_main]

use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_IAC, CODEC_ID_IMC};
use symphonia::default::codecs::ImcDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects IMC or IAC, the channel count and the sample rate (IAC derives its
    // tables from it); the rest is a sequence of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let codec = if select & 1 == 0 { CODEC_ID_IMC } else { CODEC_ID_IAC };
    let layout = if select & 2 == 0 { CHANNEL_LAYOUT_MONO } else { CHANNEL_LAYOUT_STEREO };
    let rate = [8000, 11025, 22050, 44100, 48000, 96000][usize::from(select >> 2) % 6];

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec).with_sample_rate(rate).with_channels(layout);

    fuzz_audio_decoder_packets::<ImcDecoder>(&codec_params, packets);
});
