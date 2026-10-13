#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_OPUS};
use symphonia::default::codecs::OpusAudioDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The Opus decoder requires an OpusHead (19 bytes for mapping family 0, more for others).
    // Use a fuzzer-chosen header length so mapping tables are explored too.
    if data.is_empty() {
        return;
    }
    let header_len = (19 + usize::from(data[0] % 32)).min(data.len() - 1);
    let (header, packet_data) = data[1..].split_at(header_len);

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_OPUS)
        .with_sample_rate(48000)
        .with_channels(CHANNEL_LAYOUT_STEREO)
        .with_extra_data(header.into());

    fuzz_audio_decoder::<OpusAudioDecoder>(&codec_params, packet_data.to_vec());
});
