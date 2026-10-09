// Regression tests for multichannel Vorbis decoding, multichannel Opus channel order, and the Ogg
// Opus timeline (pre-skip excluded from the timeline, exact seeks, start trim after seeking to 0).
//
// Fixtures are generated at test time with `ffmpeg` (libvorbis/libopus) and decoded with `ffmpeg`
// as the reference; tests skip when `ffmpeg` or the encoders are not available.
//
// Run with:
//   cargo test -p symphonia --release --test ogg_regressions -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Command;

use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::Time;

fn have_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(name))
        .unwrap_or(false)
}

fn run_ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(args).status().expect("failed to spawn ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

fn fixtures_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("symphonia-ogg-regression-fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds `lavfi` input arguments for `n` channels of noise. Channels 0/1 share a seed so they
/// are strongly correlated (which makes the encoder use channel coupling), and a gated tone is
/// mixed in so the encoder switches between block sizes.
fn noise_args(n: usize, layout: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut labels = String::new();

    for ch in 0..n {
        let seed = if ch == 1 { 1 } else { ch + 1 };
        args.push("-f".to_string());
        args.push("lavfi".to_string());
        args.push("-i".to_string());
        args.push(format!(
            "anoisesrc=d=3:c=pink:r=48000:a=0.25:seed={seed},\
             volume='1+0.9*sin(2*PI*3*t)':eval=frame"
        ));
        labels.push_str(&format!("[{ch}:a]"));
    }

    args.push("-filter_complex".to_string());
    args.push(format!("{labels}amerge=inputs={n},aformat=channel_layouts={layout}[a]"));
    args.push("-map".to_string());
    args.push("[a]".to_string());
    args
}

fn encode(name: &str, ext: &str, n: usize, layout: &str, codec_args: &[&str]) -> PathBuf {
    let path = fixtures_dir().join(format!("{name}.{ext}"));
    let mut args: Vec<String> = vec!["-y".into(), "-v".into(), "error".into()];
    args.extend(noise_args(n, layout));
    args.extend(codec_args.iter().map(|s| s.to_string()));
    args.push(path.to_str().unwrap().to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ffmpeg(&args);
    path
}

fn ffmpeg_decode_f32(path: &Path, decoder: &str) -> Vec<f32> {
    let raw_path = path.with_extension("ref.f32");
    // libopus' ffmpeg wrapper decodes to 16-bit (with soft clipping) by default. Request float
    // output, which is the unclipped decode that Symphonia produces.
    run_ffmpeg(&[
        "-y",
        "-v",
        "error",
        "-request_sample_fmt",
        "flt",
        "-c:a",
        decoder,
        "-i",
        path.to_str().unwrap(),
        "-f",
        "f32le",
        "-acodec",
        "pcm_f32le",
        raw_path.to_str().unwrap(),
    ]);
    let bytes = std::fs::read(&raw_path).unwrap();
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

fn open(path: &Path) -> Box<dyn FormatReader + '_> {
    let src = std::fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let hint = Hint::new();
    symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .expect("probe failed")
}

fn make_decoder(format: &dyn FormatReader) -> Box<dyn AudioDecoder> {
    let track = format.default_track(TrackType::Audio).expect("no audio track");
    symphonia::default::get_codecs()
        .make_audio_decoder(
            track.codec_params.as_ref().unwrap().audio().unwrap(),
            &Default::default(),
        )
        .expect("unsupported codec")
}

/// Decodes all packets, returning interleaved PCM and the channel count.
fn decode_all(path: &Path) -> (Vec<f32>, usize) {
    let mut format = open(path);
    let track_id = format.default_track(TrackType::Audio).unwrap().id;
    let mut decoder = make_decoder(format.as_ref());

    let mut pcm = Vec::new();
    let mut scratch = Vec::new();
    let mut channels = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) | Err(Error::ResetRequired) => break,
            Err(e) => panic!("next_packet: {e}"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).expect("decode failed");
        channels = decoded.spec().channels().count();
        decoded.copy_to_vec_interleaved(&mut scratch);
        pcm.extend_from_slice(&scratch);
    }

    (pcm, channels)
}

/// Decodes from the current position of `format`, returning at least `frames` interleaved frames
/// starting at timestamp `required` (and the timestamp of the first returned frame). Frames are
/// positioned by each packet's `pts + trim_start`.
fn read_after_seek(
    format: &mut dyn FormatReader,
    decoder: &mut dyn AudioDecoder,
    required: i64,
    frames: usize,
    channels: usize,
) -> (Option<i64>, Vec<f32>) {
    let mut got = Vec::new();
    let mut first_pos = None;
    let mut scratch = Vec::new();

    while got.len() < frames * channels {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            _ => break,
        };
        let decoded = decoder.decode(&packet).expect("decode failed");
        decoded.copy_to_vec_interleaved(&mut scratch);

        let pos = packet.pts.get() + packet.trim_start.get() as i64;
        let skip = (required - pos).max(0) as usize;

        if skip * channels >= scratch.len() {
            continue;
        }
        if first_pos.is_none() {
            first_pos = Some(pos + skip as i64);
        }
        got.extend_from_slice(&scratch[skip * channels..]);
    }

    (first_pos, got)
}

fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
    let n = reference.len().min(test.len());
    let mut signal = 0f64;
    let mut noise = 0f64;
    for i in 0..n {
        let r = f64::from(reference[i]);
        let t = f64::from(test[i]);
        signal += r * r;
        noise += (r - t) * (r - t);
    }
    if noise <= 1e-30 { f64::INFINITY } else { 10.0 * (signal / noise).log10() }
}

fn check_multichannel(
    name: &str,
    ext: &str,
    n: usize,
    layout: &str,
    codec_args: &[&str],
    ref_decoder: &str,
    min_snr: f64,
) {
    let codec = if ext == "ogg" { "libvorbis" } else { "libopus" };
    if !have_encoder(codec) {
        eprintln!("{codec} not available; skipping {name}");
        return;
    }

    let path = encode(name, ext, n, layout, codec_args);
    let reference = ffmpeg_decode_f32(&path, ref_decoder);
    let (pcm, channels) = decode_all(&path);

    assert_eq!(channels, n, "{name}: channel count");
    assert_eq!(reference.len(), pcm.len(), "{name}: frame count differs from ffmpeg");

    // The audio buffer spec must be positioned for these layouts, in the same order as ffmpeg's
    // (WAVE order) output.
    let snr = snr_db(&reference, &pcm);
    println!("{name}: {n}ch SNR={snr:.1} dB");
    assert!(snr >= min_snr, "{name}: SNR {snr:.1} dB < {min_snr} dB");
}

#[test]
fn vorbis_3ch() {
    check_multichannel(
        "vorbis_3ch",
        "ogg",
        3,
        "3.0",
        &["-c:a", "libvorbis", "-q:a", "4"],
        "vorbis",
        60.0,
    );
}

#[test]
fn vorbis_quad() {
    check_multichannel(
        "vorbis_quad",
        "ogg",
        4,
        "quad",
        &["-c:a", "libvorbis", "-q:a", "4"],
        "vorbis",
        60.0,
    );
}

#[test]
fn vorbis_5_1() {
    check_multichannel(
        "vorbis_5_1",
        "ogg",
        6,
        "5.1",
        &["-c:a", "libvorbis", "-q:a", "4"],
        "vorbis",
        60.0,
    );
}

#[test]
fn vorbis_7_1() {
    check_multichannel(
        "vorbis_7_1",
        "ogg",
        8,
        "7.1",
        &["-c:a", "libvorbis", "-q:a", "4"],
        "vorbis",
        60.0,
    );
}

#[test]
fn opus_family1_3ch() {
    check_multichannel(
        "opus_3ch",
        "opus",
        3,
        "3.0",
        &["-c:a", "libopus", "-b:a", "192k", "-mapping_family", "1"],
        "libopus",
        120.0,
    );
}

#[test]
fn opus_family1_5_1() {
    check_multichannel(
        "opus_5_1",
        "opus",
        6,
        "5.1",
        &["-c:a", "libopus", "-b:a", "256k", "-mapping_family", "1"],
        "libopus",
        120.0,
    );
}

#[test]
fn opus_family1_7_1() {
    check_multichannel(
        "opus_7_1",
        "opus",
        8,
        "7.1",
        &["-c:a", "libopus", "-b:a", "384k", "-mapping_family", "1"],
        "libopus",
        120.0,
    );
}

#[test]
fn opus_family1_is_positioned() {
    if !have_encoder("libopus") {
        return;
    }
    let path = encode(
        "opus_5_1_spec",
        "opus",
        6,
        "5.1",
        &["-c:a", "libopus", "-b:a", "256k", "-mapping_family", "1"],
    );
    let format = open(&path);
    let decoder = make_decoder(format.as_ref());
    let spec_channels =
        decoder.codec_params().channels.clone().expect("probe must report the channels");
    assert!(matches!(spec_channels, Channels::Positioned(_)), "{spec_channels:?}");
}

/// Opus timeline: `num_frames` excludes the pre-skip, and seeks by time land on the right audio.
#[test]
fn opus_timeline_excludes_pre_skip() {
    if !have_encoder("libopus") {
        eprintln!("libopus not available; skipping");
        return;
    }

    let path = encode("opus_timeline", "opus", 2, "stereo", &["-c:a", "libopus", "-b:a", "128k"]);
    let (full, channels) = decode_all(&path);
    let total_frames = full.len() / channels;

    let mut format = open(&path);
    let track = format.default_track(TrackType::Audio).unwrap().clone();
    let delay = u64::from(track.delay.expect("opus pre-skip is the track delay"));
    assert!(delay > 0);
    assert_eq!(track.num_frames, Some(total_frames as u64), "num_frames must exclude pre-skip");
    assert!(track.start_ts.get() < 0, "delay frames have negative timestamps");

    let mut decoder = make_decoder(format.as_ref());

    for secs in [0.0, 0.5, 1.0, 2.25] {
        let required = (secs * 48_000.0) as usize;

        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time { time: Time::try_from_secs_f64(secs).unwrap(), track_id: None },
            )
            .expect("seek failed");
        assert_eq!(seeked.required_ts.get(), required as i64, "required ts at {secs}s");
        decoder.reset();

        let (first_pos, got) =
            read_after_seek(format.as_mut(), decoder.as_mut(), required as i64, 4800, channels);

        assert_eq!(first_pos, Some(required as i64), "first frame position after seek to {secs}s");

        let n = (4800 * channels).min(full.len() - required * channels);
        let snr = snr_db(&full[required * channels..][..n], &got[..n]);
        println!("seek {secs}s: SNR vs continuous decode = {snr:.1} dB");
        // A seek to the start must be exact. Elsewhere the decoder state has only had the 80 ms
        // pre-roll to converge, so it is not bit-exact; but a misaligned (even by one frame)
        // noise signal would score ~0 dB.
        let min_snr = if secs == 0.0 { 100.0 } else { 20.0 };
        assert!(snr > min_snr, "seek to {secs}s: SNR {snr:.1} dB");
    }
}

/// Cross-checks the externally generated multichannel samples against `ffmpeg`. Set
/// `RMPD_SAMPLES` to the sample directory; skipped when unavailable.
#[test]
fn external_multichannel_samples() {
    let Ok(dir) = std::env::var("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };

    let files = [
        ("vorbis/vorbis_3ch.ogg", "vorbis"),
        ("vorbis/vorbis_quad.ogg", "vorbis"),
        ("vorbis/vorbis_5_1.ogg", "vorbis"),
        ("vorbis/vorbis_7_1.ogg", "vorbis"),
        ("opus/opus_ch3.opus", "libopus"),
        ("opus/opus_ffm_6ch.opus", "libopus"),
        ("opus/opus_ffm_8ch.opus", "libopus"),
    ];

    for (file, ref_decoder) in files {
        let path = Path::new(&dir).join(file);
        if !path.exists() {
            eprintln!("{file} missing; skipping");
            continue;
        }
        let reference = ffmpeg_decode_f32(&path, ref_decoder);
        let (pcm, _) = decode_all(&path);
        assert_eq!(reference.len(), pcm.len(), "{file}: length");
        let snr = snr_db(&reference, &pcm);
        println!("{file}: SNR={snr:.1} dB");
        assert!(snr > 100.0, "{file}: SNR {snr:.1} dB");
    }
}

#[test]
fn opus_family_255_and_2() {
    if !have_encoder("libopus") {
        return;
    }
    for family in ["255", "2"] {
        check_multichannel(
            &format!("opus_family{family}"),
            "opus",
            4,
            "quad",
            &["-c:a", "libopus", "-b:a", "192k", "-mapping_family", family],
            "libopus",
            120.0,
        );
    }
}

/// Decoding a cut stream from a cold decoder is identical to libopus (including the warm-up
/// frames), i.e. the difference of a seek with an 80 ms pre-roll to a continuous decode is
/// inherent to the format (CELT energy prediction converges geometrically).
#[test]
fn opus_cold_start_matches_libopus() {
    if !have_encoder("libopus") {
        return;
    }
    let path = encode("opus_cold", "opus", 2, "stereo", &["-c:a", "libopus", "-b:a", "128k"]);
    let cut = fixtures_dir().join("opus_cold_cut.opus");
    run_ffmpeg(&[
        "-y",
        "-v",
        "error",
        "-ss",
        "1.0",
        "-i",
        path.to_str().unwrap(),
        "-c:a",
        "copy",
        cut.to_str().unwrap(),
    ]);
    let reference = ffmpeg_decode_f32(&cut, "libopus");
    let (pcm, _) = decode_all(&cut);
    assert_eq!(reference.len(), pcm.len());
    let snr = snr_db(&reference, &pcm);
    println!("cold start SNR vs libopus = {snr:.1} dB");
    assert!(snr > 120.0, "SNR {snr:.1} dB");
}

fn concat(name: &str, parts: &[&Path]) -> PathBuf {
    let path = fixtures_dir().join(name);
    let mut data = Vec::new();
    for part in parts {
        data.extend(std::fs::read(part).unwrap());
    }
    std::fs::write(&path, data).unwrap();
    path
}

fn encode_stereo(name: &str, ext: &str, secs: &str, rate: &str, codec_args: &[&str]) -> PathBuf {
    let mut args = vec!["-t", secs, "-ar", rate];
    args.extend_from_slice(codec_args);
    encode(name, ext, 2, "stereo", &args)
}

/// Seeks (by chain time) to `secs`, handling the `ResetRequired` returned when the seek switches
/// links. Returns the number of `ResetRequired` errors.
fn seek_chain(
    format: &mut dyn FormatReader,
    decoder: &mut Box<dyn AudioDecoder>,
    secs: f64,
) -> (usize, symphonia::core::formats::SeekedTo) {
    let mut resets = 0;
    loop {
        let to = SeekTo::Time { time: Time::try_from_secs_f64(secs).unwrap(), track_id: None };
        match format.seek(SeekMode::Accurate, to) {
            Ok(seeked) => {
                decoder.reset();
                return (resets, seeked);
            }
            Err(Error::ResetRequired) => {
                resets += 1;
                assert!(resets < 2, "seek must complete after one reset");
                *decoder = make_decoder(&*format);
            }
            Err(e) => panic!("seek to {secs}s failed: {e}"),
        }
    }
}

fn check_chain(name: &str, ext: &str, codec_args: &[&str], rates: [&str; 2], exact_db: f64) {
    let a = encode_stereo(&format!("{name}_a"), ext, "2", rates[0], codec_args);
    let b = encode_stereo(&format!("{name}_b"), ext, "3", rates[1], codec_args);
    let chain = concat(&format!("{name}_chain.{ext}"), &[&a, &b]);

    let (full_a, _) = decode_all(&a);
    let (full_b, _) = decode_all(&b);
    let rate_a = rates[0].parse::<usize>().unwrap();
    let rate_b = rates[1].parse::<usize>().unwrap();
    assert_eq!(full_a.len() / 2, 2 * rate_a);
    assert_eq!(full_b.len() / 2, 3 * rate_b);

    let mut format = open(&chain);

    // The media information describes the whole chain.
    let info = format.media_info().clone();
    let tb = info.time_base.expect("chain time base");
    let total = tb.calc_duration(info.duration.expect("chain duration")).unwrap().as_secs_f64();
    assert!((total - 5.0).abs() < 1e-3, "chain duration {total}");

    // The first link is current.
    let mut decoder = make_decoder(&*format);
    assert_eq!(format.default_track(TrackType::Audio).unwrap().num_frames, Some(2 * rate_a as u64));

    // Seek into the second link: (time, link frames, link rate).
    let (resets, seeked) = seek_chain(format.as_mut(), &mut decoder, 3.0);
    assert_eq!(resets, 1, "seeking into another link requires one reset");
    let required = rate_b as i64;
    assert_eq!(seeked.required_ts.get(), required);
    assert_eq!(format.default_track(TrackType::Audio).unwrap().num_frames, Some(3 * rate_b as u64));
    let (first, got) = read_after_seek(format.as_mut(), decoder.as_mut(), required, 4800, 2);
    assert_eq!(first, Some(required));
    let n = got.len().min(4800 * 2);
    let snr = snr_db(&full_b[required as usize * 2..][..n], &got[..n]);
    println!("{name}: seek into link 2 SNR {snr:.1} dB");
    assert!(snr > exact_db, "link 2 SNR {snr:.1} dB");

    // Seek within the same link: no reset.
    let (resets, seeked) = seek_chain(format.as_mut(), &mut decoder, 4.5);
    assert_eq!(resets, 0);
    let required = (2.5 * rate_b as f64) as i64;
    assert_eq!(seeked.required_ts.get(), required);
    let (first, got) = read_after_seek(format.as_mut(), decoder.as_mut(), required, 4800, 2);
    assert_eq!(first, Some(required));
    let n = got.len().min(4800 * 2);
    let snr = snr_db(&full_b[required as usize * 2..][..n], &got[..n]);
    assert!(snr > exact_db, "link 2 (same link) SNR {snr:.1} dB");

    // Read to the end of the chain, then seek back into the first link.
    while let Ok(Some(_)) = format.next_packet() {}
    let (resets, seeked) = seek_chain(format.as_mut(), &mut decoder, 0.5);
    assert_eq!(resets, 1);
    let required = rate_a as i64 / 2;
    assert_eq!(seeked.required_ts.get(), required);
    let (first, got) = read_after_seek(format.as_mut(), decoder.as_mut(), required, 4800, 2);
    assert_eq!(first, Some(required));
    let n = got.len().min(4800 * 2);
    let snr = snr_db(&full_a[required as usize * 2..][..n], &got[..n]);
    println!("{name}: seek back into link 1 SNR {snr:.1} dB");
    assert!(snr > exact_db, "link 1 SNR {snr:.1} dB");

    // Seeking past the end of the chain is out-of-range.
    let to = SeekTo::Time { time: Time::try_from_secs_f64(5.5).unwrap(), track_id: None };
    assert!(matches!(
        format.seek(SeekMode::Accurate, to),
        Err(Error::SeekError(symphonia::core::errors::SeekErrorKind::OutOfRange))
    ));

    // Sequential decoding still signals each link with `ResetRequired`, exactly once.
    let mut format = open(&chain);
    let mut resets = 0;
    let mut frames = 0u64;
    loop {
        match format.next_packet() {
            Ok(Some(p)) => frames += p.dur.get(),
            Ok(None) => break,
            Err(Error::ResetRequired) => resets += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(resets, 1);
    assert_eq!(frames, (2 * rate_a + 3 * rate_b) as u64);
}

#[test]
fn chained_vorbis_seeking() {
    if !have_encoder("libvorbis") {
        return;
    }
    check_chain(
        "chain_vorbis",
        "ogg",
        &["-c:a", "libvorbis", "-q:a", "4"],
        ["44100", "48000"],
        60.0,
    );
}

#[test]
fn chained_opus_seeking() {
    if !have_encoder("libopus") {
        return;
    }
    // After a cold start the Opus decoder converges within a few hundred ms (see above), so only
    // require the correct alignment here.
    check_chain(
        "chain_opus",
        "opus",
        &["-c:a", "libopus", "-b:a", "128k"],
        ["48000", "48000"],
        20.0,
    );
}

/// The externally generated chained streams: the chain duration covers all links and a seek into
/// the second link works.
#[test]
fn external_chained_samples() {
    let Ok(dir) = std::env::var("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };

    for (file, expected_secs) in [
        ("vorbis/vorbis_chained_same.ogg", 60.0),
        ("vorbis/vorbis_chained_diff.ogg", 60.0),
        ("opus/opus_chained.opus", 60.0),
    ] {
        let path = Path::new(&dir).join(file);
        if !path.exists() {
            eprintln!("{file} missing; skipping");
            continue;
        }

        let mut format = open(&path);
        let info = format.media_info().clone();
        let secs = info.time_base.unwrap().calc_duration(info.duration.unwrap()).unwrap();
        println!("{file}: chain duration {:.3}s", secs.as_secs_f64());
        assert!((secs.as_secs_f64() - expected_secs).abs() < 6.0, "{file}: {secs:?}");

        let mut decoder = make_decoder(&*format);
        let (resets, seeked) = seek_chain(format.as_mut(), &mut decoder, secs.as_secs_f64() * 0.9);
        println!("{file}: resets={resets} seeked={seeked:?}");
        assert_eq!(resets, 1);
        let (first, got) =
            read_after_seek(format.as_mut(), decoder.as_mut(), seeked.required_ts.get(), 4800, 2);
        assert_eq!(first, Some(seeked.required_ts.get()));
        assert!(!got.is_empty());
    }
}
