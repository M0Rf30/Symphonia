//! AAC-LD (audio object type 23) and AAC-ELD (audio object type 39), frame lengths of 512 and 480.
//!
//! The fixtures are the first access units of streams encoded with FDK. A fixture is an audio
//! specific config and access units, each preceded by a 16-bit big-endian length (`.bin`), and the
//! decoded audio of another decoder as 16-bit samples (`.s16`): a 32-bit big-endian index of the
//! first sample of the decoded output that the reference has (the reference of ffmpeg does not have
//! the first frame), followed by the interleaved samples. The references of the 512 sample streams
//! are the output of ffmpeg (which agrees with this decoder to the precision of its float output),
//! and of the 480 sample streams that of FDK's own decoder (a fixed-point decoder, so they agree to
//! within some LSBs of 16 bits).

use symphonia_codec_aac::{AacDecoder, AdtsReader};
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::well_known::profiles::{
    CODEC_PROFILE_AAC_ELD, CODEC_PROFILE_AAC_LD,
};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

/// A MSB-first bit writer.
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
            *self.bytes.last_mut().unwrap() |= (((value >> i) & 1) as u8) << (7 - self.n_bits % 8);
            self.n_bits += 1;
        }
    }
}

/// The audio specific config and the access units of a fixture.
fn parse_bin(data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut records = vec![];
    let mut pos = 0;

    while pos < data.len() {
        let len = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
        records.push(data[pos + 2..pos + 2 + len].to_vec());
        pos += 2 + len;
    }

    let asc = records.remove(0);
    (asc, records)
}

/// The first sample of the output that the reference has, and the reference samples.
fn parse_s16(data: &[u8]) -> (usize, Vec<i16>) {
    let first = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let samples = data[4..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    (first, samples)
}

fn params_of(asc_bytes: &[u8]) -> AudioCodecParameters {
    let asc = AudioSpecificConfig::read(asc_bytes).expect("audio specific config");

    let mut params = AudioCodecParameters::new();

    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_extra_data(asc_bytes.into());

    if let Some(channels) = asc.output_channels() {
        params.with_channels(channels);
    }

    params
}

/// Decode the access units, which have the duration `frame_len`. Returns the interleaved
/// samples and the (rate, channels, frames) of the buffers.
fn decode_all(
    params: &AudioCodecParameters,
    frame_len: usize,
    aus: &[Vec<u8>],
) -> (Vec<f32>, Vec<(u32, usize, usize)>) {
    let mut decoder = AacDecoder::try_new(params, &AudioDecoderOptions::default()).unwrap();

    let mut samples = vec![];
    let mut formats = vec![];

    for (i, au) in aus.iter().enumerate() {
        let packet = Packet::new(
            0,
            Timestamp::new((i * frame_len) as i64),
            Duration::new(frame_len as u64),
            au.clone().into_boxed_slice(),
        );

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        let n_ch = buf.spec().channels().count();
        formats.push((buf.spec().rate(), n_ch, buf.frames()));

        for f in 0..buf.frames() {
            for c in 0..n_ch {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    (samples, formats)
}

/// Compares decoded audio with a fixture reference, returns the (SNR in dB, maximum difference in
/// LSBs) over the samples the reference has.
fn compare(
    samples: &[f32],
    channels: usize,
    first: usize,
    reference: &[i16],
    scale: f32,
) -> (f64, f32) {
    let start = first * channels;
    let n = reference.len().min(samples.len() - start);

    let mut signal = 0.0f64;
    let mut noise = 0.0f64;
    let mut max_diff = 0.0f32;

    for (i, &r) in reference.iter().enumerate().take(n) {
        let expected = f32::from(r);
        let actual = (samples[start + i] * scale).round().clamp(-32768.0, 32767.0);
        let diff = (actual - expected).abs();

        signal += f64::from(expected) * f64::from(expected);
        noise += f64::from(samples[start + i] * scale - expected).powi(2);
        max_diff = max_diff.max(diff);
    }

    (10.0 * (signal / noise.max(1e-9)).log10(), max_diff)
}

fn check_fixture(
    bin: &[u8],
    s16: &[u8],
    frame_len: usize,
    channels: usize,
    rate: u32,
    scale: f32,
    min_snr: f64,
    max_diff: f32,
) {
    let (asc_bytes, aus) = parse_bin(bin);
    let params = params_of(&asc_bytes);

    assert_eq!(params.sample_rate, Some(rate));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels));

    let (samples, formats) = decode_all(&params, frame_len, &aus);

    for format in &formats {
        assert_eq!(*format, (rate, channels, frame_len));
    }

    let (first, reference) = parse_s16(s16);
    let (snr, diff) = compare(&samples, channels, first, &reference, scale);

    assert!(snr >= min_snr, "SNR {snr} dB, expected at least {min_snr} dB");
    assert!(diff <= max_diff, "maximum difference {diff} LSB, expected at most {max_diff}");
}

#[test]
fn aac_ld_512_matches_ffmpeg() {
    // The reference is quantised with a scale of 32767.
    check_fixture(
        include_bytes!("fixtures/aac_ld_512_mono.bin"),
        include_bytes!("fixtures/aac_ld_512_mono.s16"),
        512,
        1,
        44_100,
        32767.0,
        80.0,
        1.0,
    );
}

#[test]
fn aac_eld_512_matches_ffmpeg() {
    check_fixture(
        include_bytes!("fixtures/aac_eld_512_mono.bin"),
        include_bytes!("fixtures/aac_eld_512_mono.s16"),
        512,
        1,
        44_100,
        32767.0,
        80.0,
        1.0,
    );
}

#[test]
fn aac_ld_480_matches_fdk() {
    check_fixture(
        include_bytes!("fixtures/aac_ld_480_mono.bin"),
        include_bytes!("fixtures/aac_ld_480_mono.s16"),
        480,
        1,
        44_100,
        32768.0,
        60.0,
        128.0,
    );

    check_fixture(
        include_bytes!("fixtures/aac_ld_480_stereo.bin"),
        include_bytes!("fixtures/aac_ld_480_stereo.s16"),
        480,
        2,
        48_000,
        32768.0,
        60.0,
        128.0,
    );
}

#[test]
fn aac_eld_480_matches_fdk() {
    check_fixture(
        include_bytes!("fixtures/aac_eld_480_mono.bin"),
        include_bytes!("fixtures/aac_eld_480_mono.s16"),
        480,
        1,
        44_100,
        32768.0,
        60.0,
        128.0,
    );

    check_fixture(
        include_bytes!("fixtures/aac_eld_480_stereo.bin"),
        include_bytes!("fixtures/aac_eld_480_stereo.s16"),
        480,
        2,
        48_000,
        32768.0,
        60.0,
        128.0,
    );
}

/// The audio specific config of AAC-LD (`ld`) or AAC-ELD at 44.1 kHz, with the channel
/// configuration `channel_config`, a frame length of 480 if `short_frame`, and the resilience flags
/// `resilience` (section data, scalefactor data, spectral data).
fn er_asc(ld: bool, channel_config: u32, short_frame: bool, resilience: u32) -> Vec<u8> {
    let mut bw = BitWriter::default();

    if ld {
        bw.put(23, 5);
        bw.put(4, 4);
        bw.put(channel_config, 4);
        // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder, extensionFlag.
        bw.put(short_frame as u32, 1);
        bw.put(0, 1);
        bw.put(1, 1);
        bw.put(resilience, 3);
        bw.put(0, 1); // extensionFlag3
    }
    else {
        bw.put(31, 5);
        bw.put(39 - 32, 6);
        bw.put(4, 4);
        bw.put(channel_config, 4);
        // ELDSpecificConfig
        bw.put(short_frame as u32, 1);
        bw.put(resilience, 3);
        bw.put(0, 1); // ldSbrPresentFlag
        bw.put(0, 4); // ELDEXT_TERM
    }

    bw.put(0, 2); // epConfig

    bw.bytes
}

/// A frame of one SCE or CPE, in which every band is zero.
fn silent_frame(ld: bool, pair: bool) -> Vec<u8> {
    let mut bw = BitWriter::default();

    let channels = if pair { 2 } else { 1 };

    if ld {
        bw.put(0, 4); // element_instance_tag

        if pair {
            bw.put(1, 1); // common_window
            // ics_info(): reserved, ONLY_LONG_SEQUENCE, window_shape, max_sfb, predictor.
            bw.put(0, 1);
            bw.put(0, 2);
            bw.put(0, 1);
            bw.put(0, 6);
            bw.put(0, 1);
            bw.put(0, 2); // ms_mask_present
        }
    }
    else if pair {
        bw.put(0, 6); // ics_info(): max_sfb
        bw.put(0, 2); // ms_mask_present
    }

    for _ in 0..channels {
        bw.put(100, 8); // global_gain

        if !(pair) {
            if ld {
                bw.put(0, 1);
                bw.put(0, 2);
                bw.put(0, 1);
                bw.put(0, 6);
                bw.put(0, 1);
            }
            else {
                bw.put(0, 6); // ics_info(): max_sfb
            }
        }

        if ld {
            bw.put(0, 3); // pulse_data_present, tns_data_present, gain_control_data_present
        }
        else {
            bw.put(0, 1); // tns_data_present
        }
    }

    bw.bytes
}

#[test]
fn silent_frames_have_the_frame_length() {
    for (ld, pair, channel_config, short_frame) in [
        (true, false, 1, false),
        (true, false, 1, true),
        (true, true, 2, false),
        (true, true, 2, true),
        (false, false, 1, false),
        (false, false, 1, true),
        (false, true, 2, false),
        (false, true, 2, true),
    ] {
        let name = format!("ld={ld} pair={pair} short={short_frame}");
        let frame_len = if short_frame { 480 } else { 512 };

        let asc = er_asc(ld, channel_config, short_frame, 0);
        let frame = silent_frame(ld, pair);

        let (samples, formats) = decode_all(&params_of(&asc), frame_len, &vec![frame; 6]);
        let channels = if pair { 2 } else { 1 };

        assert!(formats.iter().all(|f| *f == (44_100, channels, frame_len)), "{name}");
        assert!(samples.iter().all(|&s| s == 0.0), "{name}");
    }
}

#[test]
fn reports_the_ld_and_eld_profiles() {
    for (ld, profile) in [(true, CODEC_PROFILE_AAC_LD), (false, CODEC_PROFILE_AAC_ELD)] {
        let asc = AudioSpecificConfig::read(&er_asc(ld, 1, false, 0)).unwrap();
        assert_eq!(symphonia_common::mpeg::audio::get_audio_codec_profile(&asc), Some(profile));
    }
}

#[test]
fn error_resilience_tools_are_unsupported() {
    for ld in [true, false] {
        for resilience in 1..8 {
            let params = params_of(&er_asc(ld, 1, false, resilience));
            let err = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).err();
            assert!(matches!(err, Some(Error::Unsupported(_))), "ld={ld} flags={resilience}");
        }
    }
}

#[test]
fn unsupported_channel_layouts_and_rates_are_rejected_gracefully() {
    // A channel configuration of 0 requires a program config element, which these have none of.
    let mut bw = BitWriter::default();
    bw.put(23, 5);
    bw.put(4, 4);
    bw.put(0, 4);
    bw.put(0, 3);
    bw.put(0, 2);

    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AAC).with_sample_rate(44_100).with_extra_data(bw.bytes.into());

    assert!(AacDecoder::try_new(&params, &AudioDecoderOptions::default()).is_err());
}

/// Deterministic mutation fuzz test: corrupting access units must never panic the decoder.
#[test]
fn mutation_fuzz_never_panics() {
    let mut state = 0x9e3779b9u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };

    for (bin, frame_len) in [
        (&include_bytes!("fixtures/aac_ld_512_mono.bin")[..], 512),
        (&include_bytes!("fixtures/aac_eld_512_mono.bin")[..], 512),
        (&include_bytes!("fixtures/aac_ld_480_stereo.bin")[..], 480),
        (&include_bytes!("fixtures/aac_eld_480_stereo.bin")[..], 480),
    ] {
        let (asc, aus) = parse_bin(bin);
        let params = params_of(&asc);
        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

        for trial in 0..300 {
            let mut au = aus[trial % aus.len()].clone();

            for _ in 0..1 + trial % 3 {
                let pos = next() as usize % au.len();
                au[pos] ^= 1 << (next() % 8);
            }

            if trial % 5 == 0 {
                au.truncate(next() as usize % au.len());
            }

            let packet = Packet::new(
                0,
                Timestamp::new((trial * frame_len) as i64),
                Duration::new(frame_len as u64),
                au.into_boxed_slice(),
            );

            let _ = decoder.decode(&packet);
        }
    }
}

/// The generated format-coverage samples are used if `RMPD_SAMPLES` points at them.
fn open_sample(name: &str) -> Option<MediaSourceStream<'static>> {
    let dir = std::env::var_os("RMPD_SAMPLES")?;
    let file = std::fs::File::open(std::path::Path::new(&dir).join("aac").join(name)).ok()?;
    Some(MediaSourceStream::new(Box::new(file), Default::default()))
}

#[test]
fn sample_streams_decode_with_gapless_trimming() {
    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::formats::TrackType;
    use symphonia_format_isomp4::IsoMp4Reader;

    for (name, profile) in
        [("aac_ld_fdk.m4a", CODEC_PROFILE_AAC_LD), ("aac_eld_fdk.m4a", CODEC_PROFILE_AAC_ELD)]
    {
        let Some(mss) = open_sample(name)
        else {
            return;
        };

        let mut reader = IsoMp4Reader::try_probe_new(mss, Default::default()).unwrap();
        let track = reader.default_track(TrackType::Audio).unwrap().clone();

        let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
        else {
            panic!("expected audio codec parameters");
        };

        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1), "{name}");
        assert_eq!(params.profile, Some(profile), "{name}");

        let mut opts = AudioDecoderOptions::default();
        opts.gapless = true;
        let mut decoder = AacDecoder::try_new(&params, &opts).unwrap();

        let mut frames = 0;
        let mut energy = 0.0f64;

        while let Some(packet) = reader.next_packet().unwrap() {
            let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
            else {
                panic!("expected f32 samples");
            };

            frames += buf.frames();
            energy += buf.plane(0).unwrap().iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        }

        // The sample is 30 s of audio.
        assert_eq!(frames as u64, track.num_frames.unwrap(), "{name}");
        assert_eq!(frames, 1_323_000, "{name}");
        assert!(energy / frames as f64 > 0.01, "{name}: silence");
    }
}

// An ADTS stream cannot hold AAC-LD or AAC-ELD: only the profiles of AAC (Main, LC, SSR, LTP) can
// be signalled.
#[test]
fn adts_does_not_claim_ld() {
    // An ADTS header with a profile of 1 (LC) and 44.1 kHz, followed by a block of an LD stream.
    let (_, aus) = parse_bin(include_bytes!("fixtures/aac_ld_512_mono.bin"));
    let au = &aus[1];
    let len = au.len() + 7;

    let mut data = vec![
        0xff,
        0xf1,
        0x50,
        0x40 | ((len >> 11) & 3) as u8,
        ((len >> 3) & 0xff) as u8,
        (((len & 7) << 5) | 0x1f) as u8,
        0xfc,
    ];
    data.extend_from_slice(au);

    // The reader accepts the framing of the stream (it cannot tell), the decoder fails to decode
    // the blocks as AAC-LC, without panicking.
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());

    if let Ok(mut reader) = AdtsReader::try_probe_new(mss, Default::default()) {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(44_100)
            .with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_MONO);

        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

        while let Ok(Some(packet)) = reader.next_packet() {
            let _ = decoder.decode(&packet);
        }
    }
}
