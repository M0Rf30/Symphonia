#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::default::formats::IffReader;
use symphonia_fuzz::fuzz_demuxer;

fuzz_target!(|data: Vec<u8>| {
    fuzz_demuxer!(data, |mss, fmt: &FormatOptions, _: &MetadataOptions| IffReader::try_new(
        mss,
        fmt.clone()
    ));
});
