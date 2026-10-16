#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::default::formats::AuReader;
use symphonia_fuzz::fuzz_demuxer;

fuzz_target!(|data: Vec<u8>| {
    fuzz_demuxer!(data, |mss, fmt: &FormatOptions, _: &MetadataOptions| AuReader::try_new(
        mss,
        fmt.clone()
    ));
});
