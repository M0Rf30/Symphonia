#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_SBC;
use symphonia::default::codecs::SbcDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_SBC);

    fuzz_audio_decoder_packets::<SbcDecoder>(&codec_params, data);
});
