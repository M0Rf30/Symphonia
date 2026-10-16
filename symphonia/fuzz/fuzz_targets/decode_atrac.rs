#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_ATRAC1, CODEC_ID_ATRAC3};
use symphonia::default::codecs::AtracDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the stream layout; the rest is a sequence of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.with_sample_rate(44100);

    let mono = select & 0x80 != 0;
    codec_params.with_channels(if mono { CHANNEL_LAYOUT_MONO } else { CHANNEL_LAYOUT_STEREO });

    match select & 0x7f {
        0 => {
            codec_params.for_codec(CODEC_ID_ATRAC1);
        }
        // WAVE layout: coding mode 1 (joint stereo) or 0, frame factor 1 or 2.
        n @ 1..=4 => {
            let mode = u8::from(n & 1 != 0);
            let factor = if n > 2 { 2 } else { 1 };
            let extra = [1, 0, 0, 16, 0, 0, mode, 0, 0, 0, factor, 0, 0, 0];
            codec_params.for_codec(CODEC_ID_ATRAC3).with_extra_data(extra.to_vec().into());
        }
        // RealMedia layout.
        n => {
            let mode = if n & 1 != 0 { 0x12 } else { 0x02 };
            let extra = [0, 0, 0, 4, 0x08, 0x00, 0x08, 0x8e, 0, mode];
            codec_params.for_codec(CODEC_ID_ATRAC3).with_extra_data(extra.to_vec().into());
        }
    }

    fuzz_audio_decoder_packets::<AtracDecoder>(&codec_params, packets);
});
