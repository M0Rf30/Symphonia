#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_SIPR;
use symphonia::default::codecs::SiprDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the 16 kbit/s mode (16 kHz) or the others (8 kHz); the rest is a
    // sequence of length-prefixed packets, whose sizes select the mode.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_SIPR)
        .with_sample_rate(if select & 1 == 0 { 8000 } else { 16000 })
        .with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<SiprDecoder>(&codec_params, packets);
});
