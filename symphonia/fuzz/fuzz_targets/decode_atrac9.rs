#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::audio::well_known::CODEC_ID_ATRAC9;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::default::codecs::Atrac9Decoder;
use symphonia_fuzz::fuzz_audio_decoder_packets;

fuzz_target!(|data: &[u8]| {
    // The first byte selects the channel configuration (0 mono, 1 dual mono, 2 stereo, 3 5.1,
    // 4 7.1, 5 quad); the rest is a sequence of length-prefixed packets.
    let Some((&select, packets)) = data.split_first()
    else {
        return;
    };

    let block_config = select % 6;

    // The 12 byte ATRAC9 configuration for a 48 kHz, four-frame superframe (FFmpeg's 12 byte
    // "ATRAC9 config data"): u32 version, the 0xfe magic byte, then the packed
    // sample rate index (7 = 48 kHz), channel configuration, 11 bit average frame size minus one
    // (192 bytes) and the 2 bit superframe index (2 = four frames).
    let mut extra = [0u8; 12];
    extra[0] = 1; // version
    extra[4] = 0xfe; // magic byte
    extra[5] = 0x70 | (block_config << 1); // sample rate index 7, channel config, verification 0.
    extra[6] = 0x17; // average frame size (192) and superframe index.
    extra[7] = 0xf0;

    let mut codec_params = AudioCodecParameters::new();
    codec_params
        .for_codec(CODEC_ID_ATRAC9)
        .with_sample_rate(48000)
        .with_extra_data(extra.to_vec().into());

    fuzz_audio_decoder_packets::<Atrac9Decoder>(&codec_params, packets);
});
