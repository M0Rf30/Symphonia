#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_DCA;
use symphonia::default::codecs::DcaDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // A sequence of length-prefixed packets. The decoder reads the layout from the bitstream.
    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_DCA);

    fuzz_audio_decoder_packets::<DcaDecoder>(&codec_params, data);
});
