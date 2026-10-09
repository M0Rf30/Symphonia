use symphonia_codec_aac::{AacDecoder, AdtsReader, LoasReader};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, well_known::CODEC_ID_AAC,
};
use symphonia_core::errors;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_isomp4::IsoMp4Reader;

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
            let len =
                if trial % 5 == 0 { 1 + (next() as usize) % mutated.len() } else { mutated.len() };
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

/// A minimal MSB-first bit writer for building synthetic `raw_data_block()`s.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    n_bits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.n_bits % 8 == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.n_bits % 8);
            self.n_bits += 1;
        }
    }

    /// Write an `individual_channel_stream()` with a single section of one perceptual noise
    /// substitution (PNS) band, and no other tools.
    fn noise_ics(&mut self, global_gain: u32) {
        self.put(global_gain, 8);
        // ics_info(): reserved, ONLY_LONG_SEQUENCE, window shape, max_sfb = 1, no predictor.
        self.put(0, 1);
        self.put(0, 2);
        self.put(0, 1);
        self.put(1, 6);
        self.put(0, 1);
        // section_data(): codebook 13 (NOISE_HCB), length 1.
        self.put(13, 4);
        self.put(1, 5);
        // scale_factor_data(): the initial 9-bit noise energy.
        self.put(256, 9);
        // pulse_data_present, tns_data_present, gain_control_data_present.
        self.put(0, 3);
    }
}

/// Builds a 3.0 (channelConfiguration 3: `SCE`, `CPE`) frame where every channel is a single band
/// of perceptual noise.
fn noise_frame_3p0() -> Vec<u8> {
    let mut bw = BitWriter::default();

    // ID_SCE, instance tag 0.
    bw.put(0, 3);
    bw.put(0, 4);
    bw.noise_ics(100);

    // ID_CPE, instance tag 0, no common window.
    bw.put(1, 3);
    bw.put(0, 4);
    bw.put(0, 1);
    bw.noise_ics(100);
    bw.noise_ics(100);

    // ID_END.
    bw.put(7, 3);

    bw.bytes
}

fn decode_3p0_noise(decoder: &mut AacDecoder, data: &[u8]) -> Vec<Vec<f32>> {
    use symphonia_core::audio::Audio;
    use symphonia_core::audio::GenericAudioBufferRef;
    use symphonia_core::packet::Packet;
    use symphonia_core::units::{Duration, Timestamp};

    let packet = Packet::new(0, Timestamp::new(0), Duration::new(1024), data.to_vec());

    match decoder.decode(&packet).expect("frame decodes") {
        GenericAudioBufferRef::F32(buf) => {
            (0..buf.spec().channels().count()).map(|c| buf.plane(c).unwrap().to_vec()).collect()
        }
        _ => panic!("expected f32 samples"),
    }
}

fn decoder_3p0() -> AacDecoder {
    // AAC-LC, 48 kHz, channelConfiguration 3.
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AAC).with_extra_data(Box::new([0x11, 0x98]));

    AacDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder")
}

#[test]
fn pns_noise_generator_is_shared_between_elements() {
    let data = noise_frame_3p0();
    let mut decoder = decoder_3p0();

    // Output channels in canonical order: front left, front right, front center.
    let planes = decode_3p0_noise(&mut decoder, &data);
    let (left, right, center) = (&planes[0], &planes[1], &planes[2]);

    assert!(center.iter().any(|&s| s != 0.0), "the noise band must produce output");

    // Every channel draws different numbers from the one stream-wide generator. Per-element
    // generators would produce the same noise in the `SCE` and the first channel of the `CPE`.
    assert_ne!(center, left);
    assert_ne!(left, right);
    assert_ne!(center, right);
}

#[test]
fn reset_restarts_the_pns_noise_generator() {
    let data = noise_frame_3p0();
    let mut decoder = decoder_3p0();

    let first = decode_3p0_noise(&mut decoder, &data);
    let second = decode_3p0_noise(&mut decoder, &data);
    assert_ne!(first, second, "the generator state advances between frames");

    // A seek resets the decoder; decoding from the start must reproduce the original output.
    decoder.reset();
    assert_eq!(first, decode_3p0_noise(&mut decoder, &data));
}

// Tests using the generated format-coverage samples. They are skipped unless the `RMPD_SAMPLES`
// environment variable points at the samples directory (containing `aac/*.m4a`).

fn open_sample(name: &str) -> Option<Box<dyn symphonia_core::formats::FormatReader>> {
    let dir = std::env::var_os("RMPD_SAMPLES")?;
    let path = std::path::Path::new(&dir).join("aac").join(name);
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    if name.contains("latm") {
        Some(LoasReader::try_probe_new(mss, Default::default()).expect("sample probes"))
    }
    else {
        Some(IsoMp4Reader::try_probe_new(mss, Default::default()).expect("sample probes"))
    }
}

fn sample_params(
    reader: &dyn symphonia_core::formats::FormatReader,
) -> (u32, AudioCodecParameters) {
    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::formats::TrackType;

    let track = reader.default_track(TrackType::Audio).expect("audio track");
    match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => (track.id, params.clone()),
        _ => panic!("expected audio codec parameters"),
    }
}

#[test]
fn sample_96khz_rate_comes_from_audio_specific_config() {
    let Some(reader) = open_sample("aac_lc_fdk_96k.m4a")
    else {
        return;
    };
    let (_, params) = sample_params(reader.as_ref());
    assert_eq!(params.sample_rate, Some(96_000));
}

#[test]
fn sample_he_aac_params_and_gapless_frames_describe_decoded_output() {
    use symphonia_core::formats::TrackType;

    for (name, frames) in [("heaac_v1_fdk_m4a.m4a", 1_323_000), ("heaac_v2_fdk_m4a.m4a", 1_323_000)]
    {
        let Some(mut reader) = open_sample(name)
        else {
            return;
        };
        let (id, params) = sample_params(reader.as_ref());

        // The parameters describe the decoded output: SBR rate and (PS) stereo.
        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2), "{name}");

        let track = reader.default_track(TrackType::Audio).unwrap();
        assert_eq!(track.num_frames, Some(frames), "{name}");

        // Decoding with gapless trimming yields exactly that number of frames.
        let mut opts = AudioDecoderOptions::default();
        opts.gapless = true;
        let mut decoder = AacDecoder::try_new(&params, &opts).unwrap();

        let mut total = 0;
        while let Some(packet) = reader.next_packet().unwrap() {
            if packet.track_id == id {
                total += decoder.decode(&packet).unwrap().frames();
            }
        }
        assert_eq!(total as u64, frames, "{name}");
    }
}

#[test]
fn sample_downsampled_sbr_decodes_at_core_rate() {
    let Some(mut reader) = open_sample("heaac_v1_fdk_downsampled_sbr.m4a")
    else {
        return;
    };
    let (id, params) = sample_params(reader.as_ref());
    assert_eq!(params.sample_rate, Some(44_100));

    // Disable gapless trimming so that the delay does not shorten the first packets.
    let mut opts = AudioDecoderOptions::default();
    opts.gapless = false;
    let mut decoder = AacDecoder::try_new(&params, &opts).unwrap();

    let mut checked = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id == id {
            let buf = decoder.decode(&packet).unwrap();
            assert_eq!(buf.spec().rate(), 44_100);
            assert_eq!(buf.frames(), 1024);
            checked += 1;
            if checked == 8 {
                break;
            }
        }
    }
}

/// An accurate seek must start decoding early enough for a reset decoder to produce the same
/// audio as a continuous decode at the requested position.
fn check_accurate_seek_converges(name: &str, tolerance: f32) {
    use symphonia_core::audio::{Audio, GenericAudioBufferRef};
    use symphonia_core::formats::{SeekMode, SeekTo};
    use symphonia_core::units::Time;

    fn samples(buf: GenericAudioBufferRef<'_>) -> Vec<f32> {
        let GenericAudioBufferRef::F32(buf) = buf
        else {
            panic!("expected f32 samples");
        };
        (0..buf.spec().channels().count()).flat_map(|c| buf.plane(c).unwrap().to_vec()).collect()
    }

    let Some(mut reader) = open_sample(name)
    else {
        return;
    };
    let (id, params) = sample_params(reader.as_ref());

    // Continuous decode of the whole stream, per packet.
    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut continuous = vec![];
    let mut ticks_per_packet = 0;
    let mut pts = vec![];
    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id == id {
            // The duration of a packet in the track's timebase (not trimmed by gapless playback).
            ticks_per_packet = ticks_per_packet.max(packet.dur.get() as i64);
            pts.push(packet.pts.get());
            continuous.push(samples(decoder.decode(&packet).unwrap()));
        }
    }

    for secs in [1.0, 3.7, 10.2, 12.9] {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time { time: Time::try_from_secs_f64(secs).unwrap(), track_id: Some(id) },
            )
            .unwrap();

        decoder.reset();

        // The seek must start before the target. Packets are compared once the decoder has been
        // fed at least up to the target packet.
        let preroll = ((seeked.required_ts.get() - seeked.actual_ts.get()) / ticks_per_packet) + 1;
        assert!(preroll > 1, "{name}: the seek must pre-roll");

        let mut first = None;
        let mut n = 0;
        while let Some(packet) = reader.next_packet().unwrap() {
            if packet.track_id != id {
                continue;
            }

            // Find this packet in the continuous decode by its timestamp.
            let first = *first.get_or_insert_with(|| {
                pts.iter().position(|&p| p == packet.pts.get()).expect("packet in the stream")
            });

            let out = samples(decoder.decode(&packet).unwrap());
            let index = first + n;

            if n as i64 >= preroll {
                let max_err = out
                    .iter()
                    .zip(&continuous[index])
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f32::max);
                assert!(max_err <= tolerance, "{name}: packet {index} differs by {max_err}");
            }

            n += 1;
            if n as i64 >= preroll + 4 {
                break;
            }
        }
    }
}

#[test]
fn sample_accurate_seek_converges_aac_lc() {
    check_accurate_seek_converges("aac_lc_fdk_m4a.m4a", 1e-6);
}

#[test]
fn sample_loas_stream_decodes_and_seeks() {
    // The stream mux config is not in the first frames of this stream.
    let Some(mut reader) = open_sample("aac_lc_fdk_latm.aac")
    else {
        return;
    };
    let (id, params) = sample_params(reader.as_ref());
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));

    let mut opts = AudioDecoderOptions::default();
    opts.gapless = false;
    let mut decoder = AacDecoder::try_new(&params, &opts).unwrap();

    let mut total = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        assert_eq!(packet.track_id, id);
        total += decoder.decode(&packet).unwrap().frames();
    }
    assert_eq!(total, 1_325_056);

    check_accurate_seek_converges("aac_lc_fdk_latm.aac", 1e-6);
}

#[test]
fn sample_accurate_seek_converges_he_aac() {
    for name in ["heaac_nero_sample.mp4", "heaac_v1_fdk_m4a.m4a", "heaac_v2_fdk_m4a.m4a"] {
        check_accurate_seek_converges(name, 1e-4);
    }
}
