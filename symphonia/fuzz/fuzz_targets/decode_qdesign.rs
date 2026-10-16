#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_QDM2, CODEC_ID_QDMC};
use symphonia::default::codecs::QdesignDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects QDMC vs QDM2 and the length of the extra data (a `QDCA` atom, which
    // carries the channel count and sample rate); the rest is a sequence of length-prefixed
    // packets.
    let [select, extra_len, rest @ ..] = data
    else {
        return;
    };

    let (extra, packets) = rest.split_at(usize::from(*extra_len).min(rest.len()));

    let codec = if select % 2 == 0 { CODEC_ID_QDMC } else { CODEC_ID_QDM2 };

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(codec).with_extra_data(extra.to_vec().into_boxed_slice());

    fuzz_audio_decoder_packets::<QdesignDecoder>(&codec_params, packets);
});
