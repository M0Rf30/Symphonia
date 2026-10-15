#![no_main]

use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_MACE3, CODEC_ID_MACE6};
use symphonia::default::codecs::MaceDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects MACE 3:1 or 6:1 and mono or stereo; the rest is a sequence of
    // length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let codec = if select & 1 == 0 { CODEC_ID_MACE3 } else { CODEC_ID_MACE6 };
    let layout = if select & 2 == 0 { CHANNEL_LAYOUT_MONO } else { CHANNEL_LAYOUT_STEREO };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec).with_sample_rate(22254).with_channels(layout);

    fuzz_audio_decoder_packets::<MaceDecoder>(&codec_params, packets);
});
