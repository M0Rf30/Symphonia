#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::default::formats::MpegPsReader;
use symphonia_fuzz::fuzz_demuxer;

fuzz_target!(|data: Vec<u8>| {
    fuzz_demuxer!(data, |mss, fmt: &FormatOptions, _: &MetadataOptions| MpegPsReader::try_new(
        mss,
        fmt.clone()
    ));
});
