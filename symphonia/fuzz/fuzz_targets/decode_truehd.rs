#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_TRUEHD;
use symphonia::default::codecs::TrueHdDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // A sequence of length-prefixed access units. Decoding starts at a major sync, so several
    // packets are needed to reach the state carried between them.
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_TRUEHD);

    fuzz_audio_decoder_packets::<TrueHdDecoder>(&codec_params, data);
});
