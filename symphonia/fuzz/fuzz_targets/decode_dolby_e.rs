#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_DOLBY_E;
use symphonia::default::codecs::DolbyEDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // A sequence of length-prefixed packets. The decoder reads the layout from the bitstream.
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_DOLBY_E);

    fuzz_audio_decoder_packets::<DolbyEDecoder>(&codec_params, data);
});
