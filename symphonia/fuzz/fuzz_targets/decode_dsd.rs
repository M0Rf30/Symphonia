#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::default::codecs::DsdDecoder;
use symphonia::default::formats::CODEC_TYPE_DSD;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The first 4 bytes are the optional PCM output rate extra data (0 selects pass-through),
    // the next byte selects the DSD rate; the rest is the packet.
    if data.len() < 5 {
        return;
    }
    let (header, packet_data) = data.split_at(5);

    let dsd_rate = [2_822_400, 5_644_800, 11_289_600][usize::from(header[4]) % 3];

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(dsd_rate)
        .with_channels(CHANNEL_LAYOUT_STEREO)
        .with_extra_data(header[..4].into());

    fuzz_audio_decoder::<DsdDecoder>(&codec_params, packet_data.to_vec());
});
