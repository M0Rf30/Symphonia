#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_WMAV1, CODEC_ID_WMAV2};
use symphonia::default::codecs::WmaDecoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

const SAMPLE_RATES: [u32; 8] = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 44100];

// The first byte selects the version, the channel count, the stream flags (exponent VLC, bit
// reservoir, variable block length) and the sample rate; the rest is a sequence of packets.
fuzz_target!(|data: &[u8]| {
    let Some((&cfg, data)) = data.split_first()
    else {
        return;
    };

    let v2 = cfg & 1 != 0;
    let channels = 1 + u16::from(cfg >> 1 & 1);
    let flags = u16::from(cfg >> 2 & 7);
    let sample_rate = SAMPLE_RATES[usize::from(cfg >> 5)];
    let block_align: u16 = 256;
    let avg_bytes_per_sec = u32::from(channels) * sample_rate / 4;

    // The WAVEFORMATEX structure of the stream, followed by the codec extension.
    let mut wave_format = Vec::new();
    wave_format.extend_from_slice(&if v2 { 0x0161u16 } else { 0x0160 }.to_le_bytes());
    wave_format.extend_from_slice(&channels.to_le_bytes());
    wave_format.extend_from_slice(&sample_rate.to_le_bytes());
    wave_format.extend_from_slice(&avg_bytes_per_sec.to_le_bytes());
    wave_format.extend_from_slice(&block_align.to_le_bytes());
    wave_format.extend_from_slice(&16u16.to_le_bytes());
    if v2 {
        wave_format.extend_from_slice(&10u16.to_le_bytes());
        wave_format.extend_from_slice(&[0, 0, 0, 0]);
        wave_format.extend_from_slice(&flags.to_le_bytes());
        wave_format.extend_from_slice(&[0; 4]);
    }
    else {
        wave_format.extend_from_slice(&4u16.to_le_bytes());
        wave_format.extend_from_slice(&[0, 0]);
        wave_format.extend_from_slice(&flags.to_le_bytes());
    }

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(if v2 { CODEC_ID_WMAV2 } else { CODEC_ID_WMAV1 })
        .with_extra_data(wave_format.into_boxed_slice());

    fuzz_audio_decoder_packets::<WmaDecoder>(&codec_params, data);
});
