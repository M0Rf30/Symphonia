#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_APTX, CODEC_ID_APTX_HD};
use symphonia::default::codecs::AptxDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects aptX or aptX HD.
    let Some((&select, rest)) = data.split_first()
    else {
        return;
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(if select & 1 == 0 { CODEC_ID_APTX } else { CODEC_ID_APTX_HD })
        .with_sample_rate(48000)
        .with_channels(Channels::Discrete(2));

    fuzz_audio_decoder_packets::<AptxDecoder>(&codec_params, rest);
});
