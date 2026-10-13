#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::{AudioCodecParameters, well_known::CODEC_ID_VORBIS};
use symphonia::default::codecs::VorbisDecoder;
use symphonia_fuzz::fuzz_audio_decoder;

fuzz_target!(|data: &[u8]| {
    // The Vorbis decoder requires valid identification, comment, and setup headers in
    // extra_data. The first byte selects how much of the remaining input is the headers; the
    // remainder is fed as an audio packet, so the packet decode path is reachable when the
    // fuzzer produces valid headers.
    let Some((&len_sel, rest)) = data.split_first()
    else {
        return;
    };

    let header_len = (usize::from(len_sel) * rest.len() / 255).min(rest.len());
    let (header, packet_data) = rest.split_at(header_len);

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_VORBIS);
    codec_params.extra_data = Some(header.into());

    fuzz_audio_decoder::<VorbisDecoder>(&codec_params, packet_data.to_vec());
});
