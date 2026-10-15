#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::Timestamp;
use symphonia::default::formats::SpdifReader;

fuzz_target!(|data: Vec<u8>| {
    // The first byte selects the options and the seek target.
    let Some((&select, stream)) = data.split_first()
    else {
        return;
    };

    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(stream.to_vec())), Default::default());
    let mut opts = FormatOptions::default();
    opts.prebuild_seek_index = select & 1 != 0;

    let Ok(mut reader) = SpdifReader::try_new(mss, opts)
    else {
        return;
    };

    for _ in 0..8 {
        if !matches!(reader.next_packet(), Ok(Some(_))) {
            break;
        }
    }

    let to = SeekTo::Timestamp { ts: Timestamp::new(i64::from(select) * 1536), track_id: 0 };

    if reader.seek(SeekMode::Accurate, to).is_ok() {
        while let Ok(Some(_)) = reader.next_packet() {}
    }
});
