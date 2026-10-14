pub mod demuxer;

use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::registry::RegisterableAudioDecoder;
use symphonia::core::packet::Packet;
use symphonia::core::units::{Duration, Timestamp};

pub fn fuzz_audio_decoder<D: RegisterableAudioDecoder>(
    params: &AudioCodecParameters,
    packet_data: Vec<u8>,
) {
    if let Ok(mut decoder) = D::try_registry_new(params, &Default::default()) {
        let packet = Packet::new(0, Timestamp::ZERO, Duration::ZERO, packet_data);
        let _ = decoder.decode(&packet);
    }
}

/// Decodes `data` as a sequence of packets, each prefixed by its 16-bit little-endian length, so
/// that decoders whose state carries across packets (e.g. TrueHD major syncs) are exercised.
pub fn fuzz_audio_decoder_packets<D: RegisterableAudioDecoder>(
    params: &AudioCodecParameters,
    data: &[u8],
) {
    let Ok(mut decoder) = D::try_registry_new(params, &Default::default())
    else {
        return;
    };
    let mut rest = data;
    while let Some((len, tail)) = rest.split_first_chunk::<2>() {
        let len = usize::from(u16::from_le_bytes(*len)).min(tail.len());
        let (packet_data, tail) = tail.split_at(len);
        let packet = Packet::new(0, Timestamp::ZERO, Duration::ZERO, packet_data.to_vec());
        let _ = decoder.decode(&packet);
        rest = tail;
    }
}
