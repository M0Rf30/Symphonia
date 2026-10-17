#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AC4;
use symphonia::default::codecs::Ac4Decoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // A sequence of length-prefixed AC-4 frames, with or without the 0xAC40/0xAC41 sync header.
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_AC4);

    fuzz_audio_decoder_packets::<Ac4Decoder>(&codec_params, data);
});
