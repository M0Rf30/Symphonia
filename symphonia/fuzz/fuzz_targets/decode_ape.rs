#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_MONKEYS_AUDIO};
use symphonia::default::codecs::ApeDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The APE decoder requires 6 bytes of extra data (version, compression level, channels) and
    // a bit depth. Use the first 7 bytes of the input for those, and the rest as the packet.
    if data.len() < 7 {
        return;
    }
    let (header, packet_data) = data.split_at(7);

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_MONKEYS_AUDIO)
        .with_sample_rate(44100)
        .with_channels(CHANNEL_LAYOUT_STEREO)
        .with_bits_per_sample([8, 16, 24][usize::from(header[6]) % 3])
        .with_extra_data(header[..6].into());

    fuzz_audio_decoder::<ApeDecoder>(&codec_params, packet_data.to_vec());
});
