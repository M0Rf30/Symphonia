#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AMR_NB;
use symphonia::default::codecs::AmrNbDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The input is a sequence of length-prefixed AMR frames ([TOC byte][speech bits]).
    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_AMR_NB)
        .with_sample_rate(8000)
        .with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<AmrNbDecoder>(&codec_params, data);
});
