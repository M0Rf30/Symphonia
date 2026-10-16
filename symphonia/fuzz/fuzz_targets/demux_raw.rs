#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::Timestamp;
use symphonia::default::formats::{RawPcmParams, RawPcmReader, RawPcmSampleFormat};

const FORMATS: [RawPcmSampleFormat; 10] = [
    RawPcmSampleFormat::U8,
    RawPcmSampleFormat::S8,
    RawPcmSampleFormat::S16Le,
    RawPcmSampleFormat::U16Be,
    RawPcmSampleFormat::S24Le,
    RawPcmSampleFormat::S32Be,
    RawPcmSampleFormat::F32Le,
    RawPcmSampleFormat::F64Be,
    RawPcmSampleFormat::ALaw,
    RawPcmSampleFormat::MuLaw,
];

fuzz_target!(|data: Vec<u8>| {
    // The first 5 bytes are the parameters: the format, the channel count, and the sample rate.
    if data.len() < 5 {
        return;
    }

    let params = RawPcmParams {
        sample_format: FORMATS[usize::from(data[0]) % FORMATS.len()],
        channels: u16::from(data[1]),
        sample_rate: u32::from(u16::from_be_bytes([data[2], data[3]])),
    };

    let ts = i64::from(data[4]) * 13;

    let cursor = std::io::Cursor::new(data[5..].to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let Ok(mut reader) = RawPcmReader::try_new(mss, params, FormatOptions::default())
    else {
        return;
    };

    while let Ok(Some(_)) = reader.next_packet() {}

    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 };
    if reader.seek(SeekMode::Accurate, to).is_ok() {
        while let Ok(Some(_)) = reader.next_packet() {}
    }
});
