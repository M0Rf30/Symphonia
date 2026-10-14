#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};
use symphonia::default::codecs::Ac3Decoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects AC-3 or E-AC-3; the rest is a sequence of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };
    let codec = if select & 1 == 0 { CODEC_ID_AC3 } else { CODEC_ID_EAC3 };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec);

    fuzz_audio_decoder_packets::<Ac3Decoder>(&codec_params, packets);
});
