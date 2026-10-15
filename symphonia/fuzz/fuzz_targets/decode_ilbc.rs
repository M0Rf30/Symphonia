#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_ILBC;
use symphonia::default::codecs::IlbcDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_ILBC)
        .with_sample_rate(8000)
        .with_channels(CHANNEL_LAYOUT_MONO)
        .with_frames_per_block(if data.first().is_some_and(|b| b & 1 == 1) { 240 } else { 160 });

    fuzz_audio_decoder_packets::<IlbcDecoder>(&codec_params, data);
});
