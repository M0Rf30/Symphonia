#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_LC3;
use symphonia::default::codecs::Lc3Decoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first two bytes select the sample rate, the frame duration, the number of channels, and
    // the high resolution mode. The rest is a sequence of length-prefixed packets.
    let Some((&[select, channels], packets)) = data.split_first_chunk::<2>()
    else {
        return;
    };

    const RATES: [u32; 7] = [8000, 16000, 24000, 32000, 48000, 96000, 44100];
    const FRAME_US: [u16; 4] = [2500, 5000, 7500, 10000];

    let rate = RATES[usize::from(select & 7) % RATES.len()];
    let frame_us = FRAME_US[usize::from(select >> 3 & 3)];
    let hr_mode = select & 0x20 != 0;

    let mut extra_data = Vec::new();
    extra_data.extend_from_slice(&(frame_us / 10).to_le_bytes());
    extra_data.extend_from_slice(&0u16.to_le_bytes());
    extra_data.extend_from_slice(&u16::from(hr_mode).to_le_bytes());

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_LC3)
        .with_sample_rate(rate)
        .with_channels(Channels::Discrete(1 + u16::from(channels & 1)))
        .with_extra_data(extra_data.into_boxed_slice());

    fuzz_audio_decoder_packets::<Lc3Decoder>(&codec_params, packets);
});
