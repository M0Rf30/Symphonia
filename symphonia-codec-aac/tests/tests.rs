use symphonia_codec_aac::{AacDecoder, AdtsReader};
use symphonia_format_isomp4::IsoMp4Reader;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, well_known::CODEC_ID_AAC,
};
use symphonia_core::errors;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;

fn test_decode(data: Vec<u8>) -> symphonia_core::errors::Result<()> {
    let data = std::io::Cursor::new(data);

    let mss = MediaSourceStream::new(Box::new(data), Default::default());

    let mut reader = AdtsReader::try_probe_new(mss, Default::default())?;

    let mut decoder = AacDecoder::try_new(
        AudioCodecParameters::new().for_codec(CODEC_ID_AAC),
        &AudioDecoderOptions::default(),
    )?;

    loop {
        match reader.next_packet()? {
            Some(packet) => {
                let _ = decoder.decode(&packet);
            }
            None => break,
        };
    }

    Ok(())
}

#[test]
fn invalid_channels_aac() {
    let file = vec![
        0xff, 0xf1, 0xaf, 0xce, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xfb,
        0xaf,
    ];

    let err = test_decode(file).unwrap_err();

    assert!(matches!(err, errors::Error::Unsupported(_)));
}

/// Decodes every packet of `data` (an ADTS AAC stream) as `AacDecoder` sees it, without
/// panicking. Returns the number of channels reported once the decoder has been constructed
/// (`None` if it never got that far), for the caller to sanity check the channel layout.
fn test_decode_channels(data: &[u8]) -> errors::Result<usize> {
    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default());
    let mut reader = AdtsReader::try_probe_new(mss, Default::default())?;

    let params = match &reader.tracks()[0].codec_params {
        Some(symphonia_core::codecs::CodecParameters::Audio(params)) => params.clone(),
        _ => AudioCodecParameters::new().for_codec(CODEC_ID_AAC).clone(),
    };

    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default())?;

    let mut channels = 0;
    while let Some(packet) = reader.next_packet()? {
        let audio_buf_ref = decoder.decode(&packet)?;
        channels = audio_buf_ref.spec().channels().count();
    }
    Ok(channels)
}

#[test]
fn decodes_5p1_channel_configuration_6() {
    let data = include_bytes!("fixtures/multichannel_5p1.aac");
    let channels = test_decode_channels(data).expect("5.1 stream should decode");
    assert_eq!(channels, 6);
}

#[test]
fn decodes_pce_derived_quad_layout() {
    // `channel_configuration == 0` in the AudioSpecificConfig: the layout comes from an explicit
    // `program_config_element()` inside `GASpecificConfig()` (see `AudioSpecificConfig::read`).
    // ffmpeg only emits this for MP4/ESDS (not ADTS, whose header has no channel_configuration 0
    // + in-band PCE support in this decoder; see the `ID_PCE` comment in `aac::mod::decode_ga`).
    let data = include_bytes!("fixtures/multichannel_pce_quad.mp4").to_vec();
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut reader = IsoMp4Reader::try_probe_new(mss, Default::default()).unwrap();

    let params = match &reader.tracks()[0].codec_params {
        Some(symphonia_core::codecs::CodecParameters::Audio(params)) => params.clone(),
        _ => panic!("expected an audio track"),
    };
    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
        .expect("PCE-derived quad stream should be accepted");

    let mut channels = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        let audio_buf_ref = decoder.decode(&packet).expect("PCE-derived quad stream should decode");
        channels = audio_buf_ref.spec().channels().count();
    }
    assert_eq!(channels, 4);
}

/// Deterministic mutation fuzz test: flips bits throughout each real multichannel fixture (after
/// the ADTS header, so probing still succeeds) and asserts the decoder never panics, regardless
/// of how malformed the resulting bitstream is. `AacDecoder` must return `Err`, not panic, on
/// malformed input, since rmpd feeds it network streams that may be truncated or corrupted.
#[test]
fn multichannel_mutation_fuzz_never_panics() {
    use std::panic::{self, AssertUnwindSafe};

    let fixtures: &[&[u8]] = &[include_bytes!("fixtures/multichannel_5p1.aac")];

    let mut lcg: u32 = 0xC0FF_EE01;
    let mut next = || {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        lcg
    };

    for &fixture in fixtures {
        // A range of single-bit and byte-level mutations spread across the file, including the
        // ADTS headers themselves (channel_configuration, frame length, etc).
        for trial in 0..2000u32 {
            let mut mutated = fixture.to_vec();
            let mutations = 1 + (next() % 4) as usize;
            for _ in 0..mutations {
                let idx = (next() as usize) % mutated.len();
                let mode = next() % 3;
                mutated[idx] = match mode {
                    0 => mutated[idx] ^ (1u8 << (next() % 8)),
                    1 => next() as u8,
                    _ => 0,
                };
            }
            // Also try truncating the stream at a random point.
            let len = if trial % 5 == 0 {
                1 + (next() as usize) % mutated.len()
            }
            else {
                mutated.len()
            };
            mutated.truncate(len);

            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = test_decode_channels(&mutated);
            }));

            assert!(result.is_ok(), "decoder panicked on mutated input (trial {trial})");
        }
    }
}

/// As above, but for the MP4/ESDS + `program_config_element()` path (the new PCE bitstream
/// parser added to `symphonia-common`), reached via `IsoMp4Reader` instead of `AdtsReader`.
#[test]
fn pce_mp4_mutation_fuzz_never_panics() {
    use std::panic::{self, AssertUnwindSafe};

    let fixture = include_bytes!("fixtures/multichannel_pce_quad.mp4");

    let mut lcg: u32 = 0x5EED_F00D;
    let mut next = || {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        lcg
    };

    for trial in 0..1000u32 {
        let mut mutated = fixture.to_vec();
        let mutations = 1 + (next() % 4) as usize;
        for _ in 0..mutations {
            let idx = (next() as usize) % mutated.len();
            mutated[idx] = match next() % 3 {
                0 => mutated[idx] ^ (1u8 << (next() % 8)),
                1 => next() as u8,
                _ => 0,
            };
        }
        if trial % 5 == 0 {
            let len = 1 + (next() as usize) % mutated.len();
            mutated.truncate(len);
        }

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let mss = MediaSourceStream::new(
                Box::new(std::io::Cursor::new(mutated.clone())),
                Default::default(),
            );
            let Ok(mut reader) = IsoMp4Reader::try_probe_new(mss, Default::default())
            else {
                return;
            };
            let params = match reader.tracks().first().and_then(|t| t.codec_params.as_ref()) {
                Some(symphonia_core::codecs::CodecParameters::Audio(params)) => params.clone(),
                _ => return,
            };
            let Ok(mut decoder) = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
            else {
                return;
            };
            while let Ok(Some(packet)) = reader.next_packet() {
                if decoder.decode(&packet).is_err() {
                    break;
                }
            }
        }));

        assert!(result.is_ok(), "PCE/MP4 decoder panicked on mutated input (trial {trial})");
    }
}
