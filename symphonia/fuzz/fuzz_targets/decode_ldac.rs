#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::well_known::CODEC_ID_LDAC;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::default::codecs::LdacDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the sample rate (44.1/48/88.2/96 kHz); the rest is a sequence of
    // length-prefixed packets. LDAC frames carry their own header (sync 0xaa, rate, channels),
    // so no extra data is needed beyond the stream parameters.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let sample_rate = match select & 0x3 {
        0 => 44100,
        1 => 48000,
        2 => 88200,
        _ => 96000,
    };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_LDAC).with_sample_rate(sample_rate);

    fuzz_audio_decoder_packets::<LdacDecoder>(&codec_params, packets);
});
