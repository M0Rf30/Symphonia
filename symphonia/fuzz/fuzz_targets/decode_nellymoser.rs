#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::layouts::CHANNEL_LAYOUT_MONO;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_NELLYMOSER;
use symphonia::default::codecs::NellymoserDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects one of the FLV Nellymoser sample rates; the rest is a sequence of
    // length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };
    let rate = [5512, 8000, 11025, 16000, 22050, 44100][usize::from(select) % 6];

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_NELLYMOSER)
        .with_sample_rate(rate)
        .with_channels(CHANNEL_LAYOUT_MONO);

    fuzz_audio_decoder_packets::<NellymoserDecoder>(&codec_params, packets);
});
