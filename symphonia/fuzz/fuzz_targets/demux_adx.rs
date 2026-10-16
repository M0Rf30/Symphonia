#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::registry::RegisterableAudioDecoder;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::Timestamp;
use symphonia::default::codecs::AdpcmDecoder;
use symphonia::default::formats::AdxReader;

/// A valid stream header from 8 bytes of input: channels, sample rate, total samples and cutoff.
fn header(fields: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; 0x24];
    h[0] = 0x80;
    h[3] = 0x20;
    h[4..7].copy_from_slice(&[3, 18, 4]);
    h[7] = 1 + fields[0] % 6;
    h[8..12].copy_from_slice(&(1 + u32::from_be_bytes([0, fields[1], fields[2], fields[3]])).to_be_bytes());
    h[12..16].copy_from_slice(&u32::from(u16::from_be_bytes([fields[4], fields[5]])).to_be_bytes());
    h[16..18].copy_from_slice(&fields[6..8]);
    h[0x12] = 3;
    h[0x1e..].copy_from_slice(b"(c)CRI");
    h
}

fuzz_target!(|data: Vec<u8>| {
    // The first byte selects whether the data is preceded by a valid stream header.
    let Some((&select, rest)) = data.split_first()
    else {
        return;
    };

    let stream = if select & 1 != 0 && rest.len() >= 8 {
        let mut stream = header(&rest[..8]);
        stream.extend_from_slice(&rest[8..]);
        stream
    }
    else {
        rest.to_vec()
    };

    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(stream)), Default::default());

    let Ok(mut reader) = AdxReader::try_new(mss, FormatOptions::default())
    else {
        return;
    };

    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        return;
    };

    let Ok(mut decoder) = AdpcmDecoder::try_registry_new(&params, &Default::default())
    else {
        return;
    };

    for _ in 0..4 {
        match reader.next_packet() {
            Ok(Some(packet)) => {
                let _ = decoder.decode(&packet);
            }
            _ => break,
        }
    }

    // Seek and continue.
    let to = SeekTo::Timestamp { ts: Timestamp::new(i64::from(select) * 97), track_id: 0 };
    if reader.seek(SeekMode::Accurate, to).is_ok() {
        while let Ok(Some(packet)) = reader.next_packet() {
            let _ = decoder.decode(&packet);
        }
    }
});
