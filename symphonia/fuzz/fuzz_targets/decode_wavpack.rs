#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
use symphonia::core::audio::sample::SampleFormat;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_WAVPACK};
use symphonia::default::codecs::WavPackDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The first byte selects between native WavPack blocks (no extra data) and the Matroska
    // flavour (2-byte version in extra data) and the sample format.
    let Some((&sel, packet_data)) = data.split_first()
    else {
        return;
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_WAVPACK)
        .with_sample_rate(44100)
        .with_channels(CHANNEL_LAYOUT_STEREO)
        .with_sample_format(
            [SampleFormat::S16, SampleFormat::S32, SampleFormat::F32][usize::from(sel >> 1) % 3],
        );

    if sel & 1 == 1 {
        codec_params.with_extra_data(Box::new([0x10, 0x04]));
    }

    fuzz_audio_decoder::<WavPackDecoder>(&codec_params, packet_data.to_vec());
});
