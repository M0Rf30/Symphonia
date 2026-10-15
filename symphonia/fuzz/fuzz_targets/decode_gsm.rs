#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_GSM, CODEC_ID_GSM_MS};
use symphonia::default::codecs::GsmDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    let Some((selector, payload)) = data.split_first()
    else {
        return;
    };

    // The low bit of the first byte selects between GSM 06.10 and Microsoft GSM.
    let codec_id = if selector & 1 == 0 { CODEC_ID_GSM } else { CODEC_ID_GSM_MS };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec_id).with_sample_rate(8000).with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<GsmDecoder>(&codec_params, payload);
});
