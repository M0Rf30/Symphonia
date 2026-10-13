#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_MUSEPACK};
use symphonia::default::codecs::MpcDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The Musepack decoder requires at least 21 bytes of extra data. Use the first 21 bytes of
    // the input for it, and the rest as the packet.
    if data.len() < 21 {
        return;
    }
    let (header, packet_data) = data.split_at(21);

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_MUSEPACK)
        .with_sample_rate(44100)
        .with_channels(CHANNEL_LAYOUT_STEREO)
        .with_extra_data(header.into());

    fuzz_audio_decoder::<MpcDecoder>(&codec_params, packet_data.to_vec());
});
