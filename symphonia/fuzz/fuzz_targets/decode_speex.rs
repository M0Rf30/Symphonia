#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_SPEEX;
use symphonia::default::codecs::SpeexDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

/// The size of a Speex header packet (`sizeof(SpeexHeader)`).
const SPEEX_HEADER_SIZE: usize = 80;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the mode (sample rate), channel count, and whether a fuzzer-chosen
    // Speex header follows as extra data (as from Ogg) or not (as from FLV).
    let Some((&select, rest)) = data.split_first()
    else {
        return;
    };
    let rate = [8000, 16000, 32000][usize::from(select & 3) % 3];
    let channels = 1 + u16::from((select >> 2) & 1);

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_SPEEX)
        .with_sample_rate(rate)
        .with_channels(Channels::Discrete(channels));

    let packets = if select & 0x80 != 0 && rest.len() >= SPEEX_HEADER_SIZE {
        let (header, packets) = rest.split_at(SPEEX_HEADER_SIZE);
        codec_params.with_extra_data(header.into());
        packets
    }
    else {
        rest
    };

    fuzz_audio_decoder_packets::<SpeexDecoder>(&codec_params, packets);
});
