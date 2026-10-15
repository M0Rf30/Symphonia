#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AMR_WB;
use symphonia::default::codecs::AmrWbDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_AMR_WB).with_sample_rate(16000).with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<AmrWbDecoder>(&codec_params, data);
});
