#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_MSN_SIREN, CODEC_ID_SIREN};
use symphonia::default::codecs::SirenDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the variant (Siren 7, Siren 14, MSN Siren); the rest is a sequence
    // of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };
    let (codec, rate) = match select % 3 {
        0 => (CODEC_ID_SIREN, 16000),
        1 => (CODEC_ID_SIREN, 32000),
        _ => (CODEC_ID_MSN_SIREN, 16000),
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec).with_sample_rate(rate).with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<SirenDecoder>(&codec_params, packets);
});
