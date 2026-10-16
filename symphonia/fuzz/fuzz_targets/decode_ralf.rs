#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_RALF;
use symphonia::default::codecs::RalfDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the channel count and the maximum frame size; the rest is a
    // sequence of length-prefixed packets. The `LSD:` header of the extra data is built from them
    // so that the stream is accepted.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let channels: u16 = 1 + u16::from(select & 1);
    let max_frame_size: u32 = 1 << (8 + u32::from((select >> 1) & 7));

    let mut extra = Vec::with_capacity(24);
    extra.extend_from_slice(b"LSD:");
    extra.extend_from_slice(&[0x01, 0x03]);
    extra.extend_from_slice(&[0, 0]);
    extra.extend_from_slice(&channels.to_be_bytes());
    extra.extend_from_slice(&[0, 0]);
    extra.extend_from_slice(&44100u32.to_be_bytes());
    extra.extend_from_slice(&max_frame_size.to_be_bytes());
    extra.extend_from_slice(&[0; 4]);

    let mut codec_params = AudioCodecParameters::new();
    codec_params.for_codec(CODEC_ID_RALF).with_extra_data(extra.into_boxed_slice());

    fuzz_audio_decoder_packets::<RalfDecoder>(&codec_params, packets);
});
