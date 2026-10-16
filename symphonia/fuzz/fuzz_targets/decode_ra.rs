#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_RA10, CODEC_ID_RA20};
use symphonia::default::codecs::{Ra144Decoder, Ra288Decoder};
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the codec; the rest is a sequence of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.with_sample_rate(8000).with_channels(CHANNEL_LAYOUT_MONO);

    if select & 1 == 0 {
        codec_params.for_codec(CODEC_ID_RA10);
        fuzz_audio_decoder_packets::<Ra144Decoder>(&codec_params, packets);
    }
    else {
        codec_params.for_codec(CODEC_ID_RA20);
        fuzz_audio_decoder_packets::<Ra288Decoder>(&codec_params, packets);
    }
});
