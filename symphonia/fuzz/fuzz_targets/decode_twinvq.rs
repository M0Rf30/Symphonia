#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_TWINVQ;
use symphonia::default::codecs::TwinVqDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first 12 bytes are the extra data (channels - 1, kbit/s and sample rate flag, all
    // big-endian); the rest is a sequence of length-prefixed packets.
    if data.len() < 12 {
        return;
    }
    let (extra_data, packets) = data.split_at(12);

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_TWINVQ).with_extra_data(Box::from(extra_data));

    fuzz_audio_decoder_packets::<TwinVqDecoder>(&codec_params, packets);
});
