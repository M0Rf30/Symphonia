#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::default::formats::{AptxHdReader, AptxReader, Lc3Reader, SbcReader};
use symphonia_fuzz::fuzz_demuxer;

fuzz_target!(|data: Vec<u8>| {
    // The first byte selects the reader.
    let Some((&select, stream)) = data.split_first()
    else {
        return;
    };
    let stream = stream.to_vec();
    match select % 4 {
        0 => fuzz_demuxer!(stream, |mss, fmt: &FormatOptions, _: &MetadataOptions| {
            SbcReader::try_new(mss, fmt.clone())
        }),
        1 => fuzz_demuxer!(stream, |mss, fmt: &FormatOptions, _: &MetadataOptions| {
            AptxReader::try_new(mss, fmt.clone())
        }),
        2 => fuzz_demuxer!(stream, |mss, fmt: &FormatOptions, _: &MetadataOptions| {
            AptxHdReader::try_new(mss, fmt.clone())
        }),
        _ => fuzz_demuxer!(stream, |mss, fmt: &FormatOptions, _: &MetadataOptions| {
            Lc3Reader::try_new(mss, fmt.clone())
        }),
    }
});
