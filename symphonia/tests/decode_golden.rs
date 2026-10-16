#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
//! Golden decodes of the RMPD sample corpus.
//!
//! Sample-gated: set `RMPD_SAMPLES` to the sample directory and run with `--features all`.
//!
//! * Lossless decoders (FLAC) are pinned with an FNV-1a hash over every decoded sample converted
//!   to `f32`, `i32`, and `u32`, so any change in a decoder, bit reader, or sample converter output
//!   is detected. The hashes are the same as printed by `examples/decode_bench.rs`.
//! * Lossy decoders that run through an FFT/MDCT (AAC, MP3, Vorbis) are not bit-for-bit portable:
//!   the `f32` output depends on the `rustfft` SIMD features and on the libm of the platform. They
//!   pin the decoded frame count and are compared with the PCM of `ffmpeg` (skipped when
//!   `ffmpeg` is not on `PATH`) by SNR and maximum difference, like
//!   `symphonia-codec-aac/tests/sbr_golden.rs`.
//!
//! `GOLDEN_PRINT=1` prints the measured values for regenerating the tables.

#![cfg(all(
    feature = "aac",
    feature = "flac",
    feature = "isomp4",
    feature = "mp3",
    feature = "ogg",
    feature = "vorbis"
))]

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Lossless streams: (path relative to the sample directory, FNV-1a hash, number of decoded
/// frames).
const LOSSLESS: &[(&str, u64, u64)] = &[
    ("flac/flac_16_22k_mono.flac", 0xe898c63f638c88c9, 661500),
    ("flac/flac_16_44_3ch.flac", 0x2575e1eaec99689f, 1323000),
    ("flac/flac_16_44_4ch.flac", 0x6039ce47c85bcc97, 1323000),
    ("flac/flac_16_44_6ch.flac", 0x7fa69de6968374e7, 1323000),
    ("flac/flac_16_44_8ch.flac", 0x0b6fc3e6091cd677, 1323000),
    ("flac/flac_16_44_bs16.flac", 0xd4b1ed299671ecf5, 1323000),
    ("flac/flac_16_44_bs65535.flac", 0x7552aaf874888137, 1323000),
    ("flac/flac_16_44_c0.flac", 0xe871fde7733d0b1f, 1323000),
    ("flac/flac_16_44_c5.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_16_44_c8.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_16_44_mono.flac", 0x29e1dc0efc22850f, 1323000),
    ("flac/flac_16_44_noseektable.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_16_44_padding.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_16_44_seek1s.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_16_48_st.flac", 0x5701b143956b4ca5, 1440000),
    ("flac/flac_16_8k_mono.flac", 0xb7192cff27b66257, 240000),
    ("flac/flac_24_192_st.flac", 0x727f5879186375cf, 3840000),
    ("flac/flac_24_44_st.flac", 0xd279f4dd40bfbf55, 882000),
    ("flac/flac_24_96_6ch.flac", 0xd2872fc40d33b649, 1920000),
    ("flac/flac_24_96_st.flac", 0x834571ebd267bfcf, 1920000),
    ("flac/flac_32bit.flac", 0x834571ebd267bfcf, 1920000),
    ("flac/flac_8bit.flac", 0xfa82575f2ff8a107, 1323000),
    ("flac/flac_embedded_cue.flac", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_ogg.oga", 0x6eb37cbf3c980d07, 1323000),
    ("flac/flac_ogg_ext_ogg.ogg", 0x6eb37cbf3c980d07, 1323000),
];

/// How a lossy stream is compared with ffmpeg.
#[derive(Clone, Copy)]
enum Check {
    /// The output and the ffmpeg output are aligned and have the same channel layout.
    Ffmpeg,
    /// As `Ffmpeg`, with the first `n` frames of the output dropped before the comparison: the
    /// ffmpeg output starts `n` frames later.
    FfmpegSkipFront(usize),
    /// As `Ffmpeg`, with the loose tolerances of `LOOSE_SNR_DB`/`LOOSE_MAX_DIFF`, for a stream on
    /// which the two decoders are known to differ.
    FfmpegLoose,
    /// Only the frame count is checked: ffmpeg outputs a different channel layout.
    Skip,
}

/// Minimum SNR (dB) against ffmpeg. The measured values are above 115 dB (the `f32` output of
/// both decoders is rounded differently by the FFT backends).
const MIN_SNR_DB: f64 = 100.0;
/// Maximum sample difference against ffmpeg (about 33 LSB of 16-bit audio at full scale).
const MAX_DIFF: f32 = 1e-3;
const LOOSE_SNR_DB: f64 = 40.0;
const LOOSE_MAX_DIFF: f32 = 0.2;

/// Lossy streams: (path relative to the sample directory, number of decoded frames, check).
const LOSSY: &[(&str, u64, Check)] = &[
    ("aac/aac_8ch_id_sample.m4a", 674543, Check::Ffmpeg),
    ("aac/aac_faac_adts_sample.aac", 881664, Check::FfmpegSkipFront(1024)),
    ("aac/aac_lc_fdk_22k_mono.m4a", 661500, Check::Ffmpeg),
    ("aac/aac_lc_fdk_48k.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_5_1.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_5_1_adts.aac", 1442816, Check::Ffmpeg),
    ("aac/aac_lc_fdk_7_1.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_8k_mono.m4a", 240000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_96k.m4a", 1920000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_adts.aac", 1325056, Check::Ffmpeg),
    ("aac/aac_lc_fdk_adts_crc.aac", 1325056, Check::Ffmpeg),
    ("aac/aac_lc_fdk_gapless_both.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_gapless_iso.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_latm.aac", 1325056, Check::Ffmpeg),
    ("aac/aac_lc_fdk_m4a.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_mono.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_moov_front.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_fdk_vbr3.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_3ch.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_5_1.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_5_1_adts.aac", 1441792, Check::Ffmpeg),
    ("aac/aac_lc_ffm_7_1.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_adts.aac", 1324032, Check::Ffmpeg),
    ("aac/aac_lc_ffm_is_ms.m4a", 1323000, Check::FfmpegLoose),
    ("aac/aac_lc_ffm_m4a.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_mono.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_pns.m4a", 1323000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_quad.m4a", 1440000, Check::Ffmpeg),
    ("aac/aac_lc_ffm_twoloop.m4a", 1323000, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_48k.m4a", 1440000, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_5_1.m4a", 1440000, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_adts.aac", 1329152, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_downsampled_sbr.m4a", 1323000, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_m4a.m4a", 1323000, Check::Ffmpeg),
    ("aac/heaac_v1_fdk_mono.m4a", 1323000, Check::Skip),
    ("aac/heaac_v1_fdk_vbr2.m4a", 1323000, Check::Ffmpeg),
    ("aac/heaac_v2_fdk_48k.m4a", 1440000, Check::Ffmpeg),
    ("aac/heaac_v2_fdk_adts.aac", 1331200, Check::Ffmpeg),
    ("aac/heaac_v2_fdk_m4a.m4a", 1323000, Check::Ffmpeg),
    ("aac/heaac_v2_fdk_vbr1.m4a", 1323000, Check::Ffmpeg),
    ("mp3/mp3_11k_mono.mp3", 332352, Check::Ffmpeg),
    ("mp3/mp3_16k_mono.mp3", 481536, Check::Ffmpeg),
    ("mp3/mp3_22k_mono.mp3", 661500, Check::Ffmpeg),
    ("mp3/mp3_24k.mp3", 720000, Check::Ffmpeg),
    ("mp3/mp3_32k.mp3", 960000, Check::Ffmpeg),
    ("mp3/mp3_48k.mp3", 1440000, Check::Ffmpeg),
    ("mp3/mp3_8k_mono.mp3", 241344, Check::Ffmpeg),
    ("mp3/mp3_abr192.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_allshort.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_cbr128.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_cbr320.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_cbr64_mono.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_copyright_orig.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_crc.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_dualmono.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_ffmpeg_noxing.mp3", 1324800, Check::Ffmpeg),
    ("mp3/mp3_ffmpeg_xing.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_force_ms.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_free_format.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_joint.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_noshort.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_notag_cbr.mp3", 1324800, Check::Ffmpeg),
    ("mp3/mp3_notag_vbr.mp3", 1324800, Check::Ffmpeg),
    ("mp3/mp3_preemph.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_sample_albumart.mp3", 673920, Check::Ffmpeg),
    ("mp3/mp3_sample_ascii.mp3", 698112, Check::Ffmpeg),
    ("mp3/mp3_sample_broken_first_frame.mp3", 186624, Check::Ffmpeg),
    ("mp3/mp3_sample_jpg_in_mp3.mp3", 9677304, Check::Ffmpeg),
    ("mp3/mp3_sample_misidentified.mp3", 9667584, Check::Ffmpeg),
    ("mp3/mp3_sample_misidentified2.mp3", 17823744, Check::Ffmpeg),
    ("mp3/mp3_stereo.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_vbr_v0.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_vbr_v2.mp3", 1323000, Check::Ffmpeg),
    ("mp3/mp3_vbr_v9.mp3", 661500, Check::Ffmpeg),
    ("vorbis/vorbis_22k_mono.ogg", 661500, Check::Ffmpeg),
    ("vorbis/vorbis_3ch.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_48k.ogg", 1440000, Check::Ffmpeg),
    ("vorbis/vorbis_5_1.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_7_1.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_8k_mono.ogg", 240000, Check::Ffmpeg),
    ("vorbis/vorbis_96k_24bit.ogg", 1920000, Check::Ffmpeg),
    ("vorbis/vorbis_chained_diff.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_chained_same.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_ffmpeg_libvorbis.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_ffmpeg_native.ogg", 1323008, Check::Ffmpeg),
    ("vorbis/vorbis_managed_cbr.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_mono.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_q-1.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_q10.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_q3.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_q6.ogg", 1323000, Check::Ffmpeg),
    ("vorbis/vorbis_quad.ogg", 1323000, Check::Ffmpeg),
];

/// Decodes `path` with the default decoder options (no gapless trimming), calling `sink` with every
/// decoded buffer. Returns the number of decoded frames and the channel count.
fn decode(path: &Path, mut sink: impl FnMut(GenericAudioBufferRef<'_>)) -> (u64, usize) {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .expect("probe");
    let track = format.default_track(TrackType::Audio).expect("audio track");
    let track_id = track.id;
    let params = track.codec_params.as_ref().unwrap().audio().unwrap().clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .expect("decoder");

    let mut frames = 0u64;
    let mut channels = 0;

    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        let Ok(buf) = decoder.decode(&packet)
        else {
            continue;
        };
        frames += buf.frames() as u64;
        channels = buf.num_planes().max(channels);
        sink(buf);
    }

    (frames, channels)
}

fn decode_hash(path: &Path) -> (u64, u64) {
    let mut hash = symphonia_testutil::Fnv::new();
    let (mut f, mut i, mut u) = (Vec::<f32>::new(), Vec::<i32>::new(), Vec::<u32>::new());
    let (frames, _) = decode(path, |buf| {
        let n = buf.samples_interleaved();
        f.resize(n, 0.0);
        i.resize(n, 0);
        u.resize(n, 0);
        buf.copy_to_slice_interleaved(&mut f);
        buf.copy_to_slice_interleaved(&mut i);
        buf.copy_to_slice_interleaved(&mut u);
        hash.put(n as u32);
        for k in 0..n {
            hash.put(f[k].to_bits());
            hash.put(i[k] as u32);
            hash.put(u[k]);
        }
    });
    (hash.finish(), frames)
}

/// Decodes `path` to interleaved `f32` with ffmpeg, which trims the encoder delay and padding like
/// the default decoder options. Returns the channel count and the samples, or `None` if ffmpeg is
/// unavailable.
fn ffmpeg_pcm(path: &Path) -> Option<(usize, Vec<f32>)> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostats", "-hide_banner", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-c:a", "pcm_f32le", "-f", "wav", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    // Walk the RIFF chunks: the length of the data chunk is unknown for a piped output.
    let wav = &out.stdout;
    let mut channels = 0;
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let len = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]);
        pos += 8;
        if id == b"fmt " {
            channels = usize::from(u16::from_le_bytes([wav[pos + 2], wav[pos + 3]]));
        }
        else if id == b"data" {
            let samples = wav[pos..]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            return Some((channels, samples));
        }
        pos += len as usize + (len as usize & 1);
    }
    None
}

#[test]
fn lossless_output_matches_golden_hashes() {
    let Some(root) = symphonia_testutil::sample_dir()
    else {
        eprintln!("RMPD_SAMPLES not set, skipping");
        return;
    };
    let print = std::env::var_os("GOLDEN_PRINT").is_some();

    let mut failures = Vec::new();
    for &(rel, hash, frames) in LOSSLESS {
        let path = root.join(rel);
        if !path.exists() {
            continue;
        }
        let (got_hash, got_frames) = decode_hash(&path);
        if print {
            println!("    (\"{rel}\", 0x{got_hash:016x}, {got_frames}),");
        }
        if (got_hash, got_frames) != (hash, frames) {
            failures.push(format!("{rel}: {got_hash:016x}/{got_frames} != {hash:016x}/{frames}"));
        }
    }

    assert!(failures.is_empty(), "decode output changed:\n{}", failures.join("\n"));
}

#[test]
fn lossy_output_matches_ffmpeg() {
    let Some(root) = symphonia_testutil::sample_dir()
    else {
        eprintln!("RMPD_SAMPLES not set, skipping");
        return;
    };
    let print = std::env::var_os("GOLDEN_PRINT").is_some();

    let mut failures = Vec::new();
    for &(rel, frames, check) in LOSSY {
        let path = root.join(rel);
        if !path.exists() {
            continue;
        }

        let mut ours = Vec::<f32>::new();
        let mut tmp = Vec::<f32>::new();
        let (got_frames, channels) = decode(&path, |buf| {
            tmp.resize(buf.samples_interleaved(), 0.0);
            buf.copy_to_slice_interleaved(&mut tmp);
            ours.extend_from_slice(&tmp);
        });

        if print {
            println!("    (\"{rel}\", {got_frames}, ..),");
        }
        if got_frames != frames {
            failures.push(format!("{rel}: {got_frames} frames != {frames}"));
        }

        let (skip, min_snr, max_diff) = match check {
            Check::Ffmpeg => (0, MIN_SNR_DB, MAX_DIFF),
            Check::FfmpegSkipFront(n) => (n * channels, MIN_SNR_DB, MAX_DIFF),
            Check::FfmpegLoose => (0, LOOSE_SNR_DB, LOOSE_MAX_DIFF),
            Check::Skip => continue,
        };

        let Some((ref_channels, reference)) = ffmpeg_pcm(&path)
        else {
            eprintln!("ffmpeg cannot decode {rel}; skipping the PCM comparison");
            continue;
        };
        if ref_channels != channels {
            failures.push(format!("{rel}: {channels} channels, ffmpeg {ref_channels}"));
            continue;
        }

        // The output of a chained stream is longer in ffmpeg: compare the common prefix, but
        // require that it covers all of the output but a trailing frame of up to 2304 samples.
        let ours = &ours[skip.min(ours.len())..];
        let n = ours.len().min(reference.len());
        if n + 2304 * channels < ours.len() {
            failures.push(format!("{rel}: ffmpeg output has {} samples, {}", reference.len(), n));
            continue;
        }
        let (mut signal, mut noise, mut diff) = (0.0f64, 0.0f64, 0.0f32);
        for (&s, &r) in ours[..n].iter().zip(&reference[..n]) {
            signal += f64::from(r).powi(2);
            noise += f64::from(s - r).powi(2);
            diff = diff.max((s - r).abs());
        }
        let snr = 10.0 * (signal / noise.max(1e-30)).log10();
        if print {
            println!("// {rel}: snr {snr:.1} dB max {diff:.6}");
        }
        if snr < min_snr || diff > max_diff {
            failures.push(format!(
                "{rel}: SNR {snr:.1} dB (min {min_snr}), max difference {diff} (max {max_diff})"
            ));
        }
    }

    assert!(failures.is_empty(), "decode output differs from ffmpeg:\n{}", failures.join("\n"));
}
