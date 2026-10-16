#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_TAK};
use symphonia::default::codecs::TakDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

/// A valid stream info block: mono, 44.1 kHz, 16 bit.
const STREAM_INFO: [u8; 10] = [0x82, 0x04, 0x80, 0x52, 0x15, 0x00, 0x40, 0x4d, 0x09, 0x02];

fuzz_target!(|data: &[u8]| {
    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_TAK)
        .with_sample_rate(44100)
        .with_channels(CHANNEL_LAYOUT_MONO)
        .with_extra_data(STREAM_INFO.as_slice().into());

    fuzz_audio_decoder::<TakDecoder>(&codec_params, data.to_vec());
});
