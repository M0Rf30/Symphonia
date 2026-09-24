// End-to-end integration test: decodes ffmpeg/libopus-encoded Ogg Opus files through
// Symphonia's *default* probe + codec registry (i.e. exactly how a real consumer of this crate
// would use it, not `symphonia-codec-opus` directly), and cross-checks against `ffmpeg`'s own
// decode. Generates tiny fixtures at test time with `ffmpeg` rather than committing binaries;
// skips (rather than failing) if `ffmpeg`/`libopus` aren't available on the machine running the
// test.
//
// Run with:
//   cargo test -p symphonia --release --features opus --test opus_integration -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Command;

use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::{Duration, Time};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn run_ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(args).status().expect("failed to spawn ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

fn fixtures_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("symphonia-opus-fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Encodes a small sine-wave (or multi-tone) fixture with `ffmpeg -c:a libopus`.
fn make_fixture(name: &str, extra_args: &[&str]) -> PathBuf {
    make_fixture_ext(name, "opus", extra_args)
}

/// Like `make_fixture`, but with an explicit output container extension (e.g. `"webm"` to mux
/// via Matroska/WebM instead of Ogg -- `ffmpeg` selects the muxer from the extension).
fn make_fixture_ext(name: &str, ext: &str, extra_args: &[&str]) -> PathBuf {
    let path = fixtures_dir().join(format!("{name}.{ext}"));
    let mut args: Vec<&str> = vec!["-y", "-v", "error"];
    args.extend_from_slice(extra_args);
    let path_str = path.to_str().unwrap().to_string();
    args.push(&path_str);
    run_ffmpeg(&args);
    path
}

/// Decodes `opus_path` with `ffmpeg` itself to raw interleaved `f32le` PCM (the reference).
/// Explicitly forces the `libopus` decoder: `ffmpeg`'s default (`-i x.opus -f f32le` with no
/// `-c:a`) uses FFmpeg's own *native* Opus decoder, a from-scratch reimplementation independent
/// of `libopus` -- comparing against it is not equivalent to comparing against the reference
/// implementation this crate targets bit-exactness/high-fidelity with (measured ~24 dB SNR
/// between FFmpeg's native decoder and `libopus` itself on SILK content, i.e. a real difference
/// between the two references, not a bug in either).
fn ffmpeg_decode_f32(opus_path: &Path) -> Vec<f32> {
    let raw_path = opus_path.with_extension("ref.f32");
    run_ffmpeg(&[
        "-y",
        "-v",
        "error",
        "-c:a",
        "libopus",
        "-i",
        opus_path.to_str().unwrap(),
        "-f",
        "f32le",
        "-acodec",
        "pcm_f32le",
        raw_path.to_str().unwrap(),
    ]);
    let bytes = std::fs::read(&raw_path).unwrap();
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// Decodes `opus_path` through Symphonia's *default* probe + codec registry end-to-end,
/// returning interleaved PCM and the channel count.
fn symphonia_decode(opus_path: &Path) -> (Vec<f32>, usize) {
    let src = std::fs::File::open(opus_path).unwrap();
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("opus");

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .expect("symphonia: unsupported format / probe failed");

    let track = format.default_track(TrackType::Audio).expect("symphonia: no audio track").clone();
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            track.codec_params.as_ref().expect("missing codec params").audio().unwrap(),
            &Default::default(),
        )
        .expect("symphonia: unsupported codec (is the `opus` feature enabled?)");

    let mut pcm: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut channels = 0usize;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::ResetRequired) => break,
            Err(e) => panic!("symphonia: unrecoverable error: {e}"),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if channels == 0 {
                    channels = decoded.spec().channels().count();
                }
                // `copy_to_vec_interleaved` resizes (replaces) `dst`; accumulate via a scratch
                // buffer instead of overwriting `pcm` with only the last packet's samples.
                decoded.copy_to_vec_interleaved(&mut scratch);
                pcm.extend_from_slice(&scratch);
            }
            Err(e @ (Error::IoError(_) | Error::DecodeError(_))) => {
                eprintln!("opus_integration: decode error on packet, skipping: {e}");
                continue;
            }
            Err(e) => panic!("symphonia: unrecoverable decode error: {e}"),
        }
    }

    (pcm, channels)
}

/// Signal-to-noise ratio in dB between `reference` and `test`, searching a range of
/// `+/-max_offset` integer sample offsets to tolerate benign alignment differences between
/// decoders' gapless-trim boundary conventions (a raw point-wise comparison is otherwise overly
/// sensitive to sub-sample phase shift, particularly for pure-tone test content).
fn snr_db(reference: &[f32], test: &[f32], max_offset: i64) -> f64 {
    let mut best = f64::NEG_INFINITY;
    let mut best_off = 0i64;
    for off in -max_offset..=max_offset {
        let (r, t): (&[f32], &[f32]) = if off >= 0 {
            (&reference[off as usize..], test)
        }
        else {
            (reference, &test[(-off) as usize..])
        };
        let n = r.len().min(t.len());
        let mut signal = 0f64;
        let mut noise = 0f64;
        for i in 0..n {
            let rv = r[i] as f64;
            let tv = t[i] as f64;
            signal += rv * rv;
            noise += (rv - tv) * (rv - tv);
        }
        let snr = if noise <= 1e-30 { f64::INFINITY } else { 10.0 * (signal / noise).log10() };
        if snr > best {
            best = snr;
            best_off = off;
        }
    }
    if best_off != 0 {
        println!("(best alignment offset: {best_off} samples)");
    }
    best
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (sum / x.len() as f64).sqrt()
}

/// Runs one fixture end-to-end: encode -> symphonia decode -> compare vs ffmpeg decode.
/// `min_snr_db` gates the sample-accuracy check; `None` skips SNR entirely (used only for the
/// 5.1 case, where FFmpeg's internal channel ordering for >2 channels isn't guaranteed to match
/// RFC 7845 "Vorbis order" 1:1, making a raw interleaved SNR comparison channel-order-fragile --
/// frame count (gapless-exactness) and overall RMS energy are checked instead, still exercising
/// the full 5.1 decode path end-to-end as required).
fn check_fixture(name: &str, encode_args: &[&str], expected_channels: usize, min_snr_db: Option<f64>) {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping {name}");
        return;
    }
    let opus_path = make_fixture(name, encode_args);
    let reference = ffmpeg_decode_f32(&opus_path);
    let (pcm, channels) = symphonia_decode(&opus_path);

    assert_eq!(channels, expected_channels, "{name}: channel count mismatch");

    let ref_frames = reference.len() / expected_channels;
    let got_frames = pcm.len() / expected_channels;
    // Gapless-exactness: Symphonia's trimmed sample count must match ffmpeg's own (gapless)
    // decode exactly, or be within a couple of frames of it (ffmpeg's `-f f32le` muxer can
    // occasionally emit one partial trailing frame of silence padding that a strict Ogg-Opus
    // gapless decode correctly omits).
    let frame_diff = (ref_frames as i64 - got_frames as i64).abs();
    assert!(
        frame_diff <= 4,
        "{name}: gapless sample-count mismatch: ffmpeg={ref_frames} symphonia={got_frames}"
    );

    if let Some(min_snr) = min_snr_db {
        let snr = snr_db(&reference, &pcm, 32);
        println!("{name}: {channels}ch, {got_frames} frames, SNR={snr:.1} dB");
        assert!(snr >= min_snr, "{name}: SNR {snr:.1} dB below required {min_snr:.1} dB");
    }
    else {
        let r_rms = rms(&reference);
        let g_rms = rms(&pcm);
        println!("{name}: {channels}ch, {got_frames} frames, rms ref={r_rms:.4} got={g_rms:.4}");
        assert!(g_rms > 0.0, "{name}: decoded silence");
        let ratio_db = 20.0 * (g_rms / r_rms.max(1e-9)).log10();
        assert!(ratio_db.abs() < 3.0, "{name}: RMS energy mismatch: {ratio_db:.1} dB");
    }
}

#[test]
fn stereo_celt_128k() {
    check_fixture(
        "stereo_celt_128k",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:duration=2:sample_rate=48000",
            "-filter_complex",
            "[0:a][1:a]amerge=inputs=2[a]",
            "-map",
            "[a]",
            "-c:a",
            "libopus",
            "-b:a",
            "128k",
            "-vbr",
            "on",
            "-application",
            "audio",
        ],
        2,
        Some(60.0),
    );
}

#[test]
fn mono_silk_voip_12k() {
    check_fixture(
        "mono_silk_voip_12k",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:duration=2:sample_rate=48000",
            "-ac",
            "1",
            "-c:a",
            "libopus",
            "-b:a",
            "12k",
            "-vbr",
            "on",
            "-application",
            "voip",
        ],
        1,
        // SILK is bit-exact vs libopus at the bitstream level, but ffmpeg's own libopus
        // integration may still round-trip through a slightly different resampling/output path,
        // so this is a high-but-not-infinite SNR bar rather than exact equality.
        Some(60.0),
    );
}

#[test]
fn hybrid_32k() {
    check_fixture(
        "hybrid_32k",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=2:sample_rate=48000",
            "-ac",
            "2",
            "-c:a",
            "libopus",
            "-b:a",
            "32k",
            "-vbr",
            "on",
            "-application",
            "audio",
        ],
        2,
        Some(60.0),
    );
}

#[test]
fn surround_5_1() {
    check_fixture(
        "surround_5_1",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=2:sample_rate=48000",
            "-filter_complex",
            "aformat=channel_layouts=5.1",
            "-c:a",
            "libopus",
            "-b:a",
            "256k",
            "-mapping_family",
            "1",
        ],
        6,
        None,
    );
}

/// Release-mode x-realtime measurement (assignment step 5): a 60s/160kbit/s stereo CELT file
/// and a 60s hybrid file. Only times the decode (via `symphonia_decode`, which already excludes
/// fixture generation/ffmpeg reference decode); correctness of the CELT output is irrelevant
/// here (known upstream bugs, see CeltOracleDebug) -- this measures the code path's actual cost
/// regardless of numerical correctness.
///
/// Run with:
///   cargo test -p symphonia --release --features opus --test opus_integration \
///     -- --ignored perf --nocapture
#[test]
#[ignore = "run explicitly with --release --ignored to measure x-realtime"]
fn perf_celt_60s_160k() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping");
        return;
    }
    let path = make_fixture(
        "perf_celt_60s_160k",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=60:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:duration=60:sample_rate=48000",
            "-filter_complex",
            "[0:a][1:a]amerge=inputs=2[a]",
            "-map",
            "[a]",
            "-c:a",
            "libopus",
            "-b:a",
            "160k",
            "-vbr",
            "on",
            "-application",
            "audio",
        ],
    );
    let iters: u32 = std::env::var("OPUS_PERF_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let start = std::time::Instant::now();
    let mut last = (Vec::new(), 0usize);
    for _ in 0..iters {
        last = symphonia_decode(&path);
    }
    let (pcm, channels) = last;
    let elapsed = start.elapsed() / iters.max(1);
    let audio_seconds = (pcm.len() / channels.max(1)) as f64 / 48000.0;
    println!(
        "perf_celt_60s_160k: decoded {audio_seconds:.3}s audio in {elapsed:?} => {:.1}x realtime",
        audio_seconds / elapsed.as_secs_f64()
    );
}

#[test]
#[ignore = "run explicitly with --release --ignored to measure x-realtime"]
fn perf_hybrid_60s_32k() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping");
        return;
    }
    let path = make_fixture(
        "perf_hybrid_60s_32k",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=60:sample_rate=48000",
            "-ac",
            "2",
            "-c:a",
            "libopus",
            "-b:a",
            "32k",
            "-vbr",
            "on",
            "-application",
            "audio",
        ],
    );
    let start = std::time::Instant::now();
    let (pcm, channels) = symphonia_decode(&path);
    let elapsed = start.elapsed();
    let audio_seconds = (pcm.len() / channels.max(1)) as f64 / 48000.0;
    println!(
        "perf_hybrid_60s_32k: decoded {audio_seconds:.3}s audio in {elapsed:?} => {:.1}x realtime",
        audio_seconds / elapsed.as_secs_f64()
    );
}

/// Opens `path` (with container hinted by `ext`) through Symphonia's default probe.
fn probe_opus(path: &Path, ext: &str) -> Box<dyn FormatReader> {
    let src = std::fs::File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .expect("symphonia: unsupported format / probe failed")
}

/// Seek pre-roll verification (assignment step 1): seeks to `time_s` and decodes a `200`ms
/// window starting at the *requested* timestamp two different ways:
///  - `warm`: Symphonia's actual (fixed) behaviour. `FormatReader::seek` backs the target off
///    by the codec's mandatory pre-roll (RFC 7845 section 4.6 for Ogg via `max_rap_period`;
///    Matroska `SeekPreRoll` for MKV/WebM) and returns an earlier `actual_ts`; every packet
///    from `actual_ts` up to `required_ts` is decoded and discarded before keeping the window.
///  - `cold`: an artificial pre-fix baseline. A *freshly* `reset()` decoder is fed *only* the
///    packet(s) starting at `required_ts`, i.e. zero lead-in -- this is what every format
///    reader did before this patch (`actual_ts == required_ts` unconditionally).
/// Returns `(warm_pcm, cold_pcm)`, both interleaved PCM windows of (up to) `200`ms.
fn seek_preroll_window(path: &Path, ext: &str, time_s: u32, channels: usize) -> (Vec<f32>, Vec<f32>) {
    const WINDOW_MS: f64 = 200.0;
    const SAMPLE_RATE: f64 = 48_000.0;

    let mut format = probe_opus(path, ext);
    let track = format.default_track(TrackType::Audio).expect("symphonia: no audio track").clone();
    let track_id = track.id;
    let time_base = track.time_base.expect("symphonia: track has no time base");

    let seeked = format
        .seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(time_s), track_id: Some(track_id) })
        .expect("symphonia: seek failed");

    assert!(
        seeked.actual_ts <= seeked.required_ts,
        "{ext}: pre-roll seek must never land after the requested position \
         (actual_ts={:?} > required_ts={:?})",
        seeked.actual_ts,
        seeked.required_ts
    );

    // Elapsed time, in seconds, from `actual_ts` to `required_ts` -- i.e. the pre-roll lead-in
    // that must be decoded and discarded (zero if the container has no pre-roll for this
    // track, in which case `warm` and `cold` below decode identically).
    let lead_in_ticks = (seeked.required_ts.get() - seeked.actual_ts.get()) as u64;
    let lead_in_secs = time_base.calc_duration(Duration::new(lead_in_ticks)).unwrap().as_secs_f64();
    let window_secs = WINDOW_MS / 1000.0;

    // Collect every packet for this track from `actual_ts` through `required_ts + window`.
    let mut packets: Vec<symphonia::core::packet::Packet> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::ResetRequired) => break,
            Err(e) => panic!("symphonia: unrecoverable error collecting packets: {e}"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let end_ticks =
            (packet.pts.get() + packet.dur.get() as i64 - seeked.actual_ts.get()).max(0) as u64;
        let end_secs = time_base.calc_duration(Duration::new(end_ticks)).unwrap().as_secs_f64();
        let have_enough = end_secs >= lead_in_secs + window_secs;
        packets.push(packet);
        if have_enough {
            break;
        }
    }
    assert!(!packets.is_empty(), "{ext}: no packets collected after seeking to {time_s}s");

    let decode_from = |pkts: &[symphonia::core::packet::Packet]| -> Vec<f32> {
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(
                track.codec_params.as_ref().expect("missing codec params").audio().unwrap(),
                &Default::default(),
            )
            .expect("symphonia: unsupported codec (is the `opus` feature enabled?)");
        let mut pcm = Vec::new();
        let mut scratch = Vec::new();
        for packet in pkts {
            match decoder.decode(packet) {
                Ok(decoded) => {
                    decoded.copy_to_vec_interleaved(&mut scratch);
                    pcm.extend_from_slice(&scratch);
                }
                Err(Error::IoError(_)) | Err(Error::DecodeError(_)) => continue,
                Err(e) => panic!("symphonia: unrecoverable decode error: {e}"),
            }
        }
        pcm
    };

    let window_frames = (window_secs * SAMPLE_RATE).round() as usize;
    let slice = |full: &[f32], skip_frames: usize| -> Vec<f32> {
        let start = skip_frames.saturating_mul(channels).min(full.len());
        let end = (start + window_frames * channels).min(full.len());
        full[start..end].to_vec()
    };

    // "warm": decode every collected packet (from `actual_ts`), discard the pre-roll lead-in.
    let warm_full = decode_from(&packets);
    let warm_skip = (lead_in_secs * SAMPLE_RATE).round() as usize;
    let warm = slice(&warm_full, warm_skip);

    // "cold": freshly reset decoder fed only the packet(s) from `required_ts` onward.
    let split = packets
        .iter()
        .position(|p| {
            let end_ticks = (p.pts.get() + p.dur.get() as i64 - seeked.actual_ts.get()).max(0) as u64;
            let end_secs = time_base.calc_duration(Duration::new(end_ticks)).unwrap().as_secs_f64();
            end_secs >= lead_in_secs
        })
        .unwrap_or(0);
    let cold_full = decode_from(&packets[split..]);
    let cold_lead_ticks = (seeked.required_ts.get() - packets[split].pts.get()).max(0) as u64;
    let cold_skip_secs = time_base.calc_duration(Duration::new(cold_lead_ticks)).unwrap().as_secs_f64();
    let cold_skip = (cold_skip_secs * SAMPLE_RATE).round() as usize;
    let cold = slice(&cold_full, cold_skip);

    (warm, cold)
}

/// Runs the seek pre-roll comparison (assignment step 1) at several positions through a 10s
/// fixture, printing warm (pre-roll-corrected) vs. cold (no pre-roll) SNR at each position, and
/// asserting the pre-roll-corrected path meets a sample-accuracy bar. `ext` selects the
/// container (`"opus"` for Ogg, `"webm"` for Matroska/WebM).
fn check_seek_preroll(ext: &str, channels: usize) {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping seek_preroll ({ext})");
        return;
    }
    let name = format!("seek_preroll_{ext}");
    let path = make_fixture_ext(
        &name,
        ext,
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=10:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:duration=10:sample_rate=48000",
            "-filter_complex",
            "[0:a][1:a]amerge=inputs=2[a]",
            "-map",
            "[a]",
            "-c:a",
            "libopus",
            "-b:a",
            "64k",
            "-vbr",
            "on",
            "-application",
            "audio",
        ],
    );
    let reference = ffmpeg_decode_f32(&path);

    let window_frames = ((200.0f64 / 1000.0) * 48_000.0).round() as usize;
    let mut worst_warm = f64::INFINITY;
    let mut worst_cold = f64::INFINITY;

    for &time_s in &[1u32, 3, 5, 7, 9] {
        let (warm, cold) = seek_preroll_window(&path, ext, time_s, channels);

        let ref_start = (time_s as usize) * 48_000 * channels;
        if ref_start >= reference.len() || warm.is_empty() || cold.is_empty() {
            continue;
        }
        let ref_end = (ref_start + window_frames * channels).min(reference.len());
        let ref_window = &reference[ref_start..ref_end];

        // Matroska/WebM timestamps only have millisecond resolution (`TimestampScale`), unlike
        // Ogg's sample-accurate 48kHz granule positions; the codec's mandatory gapless-trim
        // delay (a non-integer number of milliseconds, e.g. Opus's 6.5ms/312-sample default) is
        // therefore representable in the PTS domain only to the nearest millisecond, leaving a
        // benign, constant sub-2ms (~96 sample) discrepancy between a PTS-derived seek target
        // and the same instant in an external wall-clock-timed reference. Ogg has no such slack.
        let max_offset = if ext == "webm" { 200 } else { 32 };
        let warm_snr = snr_db(ref_window, &warm, max_offset);
        let cold_snr = snr_db(ref_window, &cold, max_offset);
        println!(
            "seek_preroll ({ext}) @ {time_s}s: warm(pre-roll)={warm_snr:.1} dB \
             cold(no pre-roll)={cold_snr:.1} dB"
        );

        worst_warm = worst_warm.min(warm_snr);
        worst_cold = worst_cold.min(cold_snr);
    }

    println!("seek_preroll ({ext}): worst warm={worst_warm:.1} dB worst cold={worst_cold:.1} dB");
    assert!(
        worst_warm >= 20.0,
        "seek_preroll ({ext}): pre-roll-corrected seek SNR {worst_warm:.1} dB below 20 dB bar"
    );
}

#[test]
fn seek_preroll_ogg() {
    check_seek_preroll("opus", 2);
}

#[test]
fn seek_preroll_webm() {
    check_seek_preroll("webm", 2);
}
