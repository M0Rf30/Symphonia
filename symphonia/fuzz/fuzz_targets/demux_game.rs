#![no_main]
use libfuzzer_sys::fuzz_target;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::registry::RegisterableAudioDecoder;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::Timestamp;
use symphonia::default::codecs::{AdpcmDecoder, HcaDecoder};
use symphonia::default::formats::{AdpReader, AfcReader, AstReader, HcaReader, VagReader};

fuzz_target!(|data: Vec<u8>| {
    // The first byte selects the format.
    let Some((&select, rest)) = data.split_first()
    else {
        return;
    };

    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(rest.to_vec())), Default::default());
    let opts = FormatOptions::default();

    let reader: Result<Box<dyn FormatReader>, _> = match select % 5 {
        0 => AdpReader::try_new(mss, opts).map(|r| Box::new(r) as Box<dyn FormatReader>),
        1 => AfcReader::try_new(mss, opts).map(|r| Box::new(r) as Box<dyn FormatReader>),
        2 => AstReader::try_new(mss, opts).map(|r| Box::new(r) as Box<dyn FormatReader>),
        3 => VagReader::try_new(mss, opts).map(|r| Box::new(r) as Box<dyn FormatReader>),
        _ => HcaReader::try_new(mss, opts).map(|r| Box::new(r) as Box<dyn FormatReader>),
    };

    let Ok(mut reader) = reader
    else {
        return;
    };

    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        return;
    };

    let decoder = if select % 5 == 4 {
        HcaDecoder::try_registry_new(&params, &Default::default())
    }
    else {
        AdpcmDecoder::try_registry_new(&params, &Default::default())
    };

    let Ok(mut decoder) = decoder
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
