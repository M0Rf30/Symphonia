#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::default::formats::TtaReader;
use symphonia_fuzz::fuzz_demuxer;

fuzz_target!(|data: Vec<u8>| {
    fuzz_demuxer!(data, |mss, fmt: &FormatOptions, _: &MetadataOptions| TtaReader::try_new(
        mss,
        fmt.clone()
    ));
});
