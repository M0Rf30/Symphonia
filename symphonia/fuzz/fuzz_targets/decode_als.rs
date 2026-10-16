#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_ALS;
use symphonia::default::codecs::AlsDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

/// The size of the fixed part of the ALSSpecificConfig.
const ALS_CONFIG_SIZE: usize = 22;

fuzz_target!(|data: &[u8]| {
    // The extra data is a fuzzer-chosen ALSSpecificConfig (the rest are packets).
    if data.len() < ALS_CONFIG_SIZE {
        return;
    }
    let (config, packets) = data.split_at(ALS_CONFIG_SIZE);

    let mut extra_data = b"ALS\0".to_vec();
    extra_data.extend_from_slice(&config[4..]);

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_ALS).with_extra_data(extra_data.into());

    fuzz_audio_decoder_packets::<AlsDecoder>(&codec_params, packets);
});
