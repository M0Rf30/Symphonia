//! Golden output hashes for full decodes of the RMPD sample corpus.
//!
//! Sample-gated: set `RMPD_SAMPLES` to the sample directory and run with `--features all`. Each
//! hash is an FNV-1a over every decoded sample converted to `f32`, `i32`, and `u32`, so any change
//! in a decoder, bit reader, or sample converter output is detected. The hashes are the same as
//! printed by `examples/decode_bench.rs`.

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

use symphonia::core::audio::Audio;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// (path relative to the sample directory, FNV-1a hash, number of decoded frames).
const GOLDEN: &[(&str, u64, u64)] = &[
    ("aac/aac_8ch_id_sample.m4a", 0xede40ccd9ffed256, 674543),
    ("aac/aac_faac_adts_sample.aac", 0x18457bd32164fb98, 881664),
    ("aac/aac_lc_fdk_22k_mono.m4a", 0x690db0e8bf5fecff, 661500),
    ("aac/aac_lc_fdk_48k.m4a", 0x9001514ea9d9588d, 1440000),
    ("aac/aac_lc_fdk_5_1.m4a", 0x6b0403eb3c2ac62b, 1440000),
    ("aac/aac_lc_fdk_5_1_adts.aac", 0xbd04cc786728bc6d, 1442816),
    ("aac/aac_lc_fdk_7_1.m4a", 0xfea30d0d01940bbb, 1440000),
    ("aac/aac_lc_fdk_8k_mono.m4a", 0xbfc30c6b8c1c3e01, 240000),
    ("aac/aac_lc_fdk_96k.m4a", 0x8b0975d1ebe554aa, 1920000),
    ("aac/aac_lc_fdk_adts.aac", 0x2cb66de2484423d6, 1325056),
    ("aac/aac_lc_fdk_adts_crc.aac", 0x74c8c57f4f83debc, 1325056),
    ("aac/aac_lc_fdk_gapless_both.m4a", 0xea93a94a5f66fc62, 1323000),
    ("aac/aac_lc_fdk_gapless_iso.m4a", 0xea93a94a5f66fc62, 1323000),
    ("aac/aac_lc_fdk_latm.aac", 0xd7fa8ddaa4accffb, 1325056),
    ("aac/aac_lc_fdk_m4a.m4a", 0xea93a94a5f66fc62, 1323000),
    ("aac/aac_lc_fdk_mono.m4a", 0x9df378641a09d169, 1323000),
    ("aac/aac_lc_fdk_moov_front.m4a", 0xea93a94a5f66fc62, 1323000),
    ("aac/aac_lc_fdk_vbr3.m4a", 0x81dc9472508cf7f1, 1323000),
    ("aac/aac_lc_ffm_3ch.m4a", 0xf3b3377b5bcbaa91, 1440000),
    ("aac/aac_lc_ffm_5_1.m4a", 0xecd4fb60577b127d, 1440000),
    ("aac/aac_lc_ffm_5_1_adts.aac", 0xde5c78ae6bb7f3e9, 1441792),
    ("aac/aac_lc_ffm_7_1.m4a", 0x73f3e2aef8fb8c14, 1440000),
    ("aac/aac_lc_ffm_adts.aac", 0x1740efc257987afc, 1324032),
    ("aac/aac_lc_ffm_is_ms.m4a", 0x1459a7562f8545c1, 1323000),
    ("aac/aac_lc_ffm_m4a.m4a", 0x4d32abdd24d5af0e, 1323000),
    ("aac/aac_lc_ffm_mono.m4a", 0x2b597018f6b3b610, 1323000),
    ("aac/aac_lc_ffm_pns.m4a", 0x0485a2c8134ebd05, 1323000),
    ("aac/aac_lc_ffm_quad.m4a", 0x20c19cc521c8dfae, 1440000),
    ("aac/aac_lc_ffm_twoloop.m4a", 0x4d32abdd24d5af0e, 1323000),
    ("aac/heaac_v1_fdk_48k.m4a", 0xa80e73cecd51439b, 1440000),
    ("aac/heaac_v1_fdk_5_1.m4a", 0x283b441c1beed511, 1440000),
    ("aac/heaac_v1_fdk_adts.aac", 0x3f1293f84f18acda, 1329152),
    ("aac/heaac_v1_fdk_downsampled_sbr.m4a", 0x8673ede467345468, 1323000),
    ("aac/heaac_v1_fdk_m4a.m4a", 0xbf6bbfdc0c592d8e, 1323000),
    ("aac/heaac_v1_fdk_mono.m4a", 0x7b7814ea3617abe0, 1323000),
    ("aac/heaac_v1_fdk_vbr2.m4a", 0xbccf435325f3f52f, 1323000),
    ("aac/heaac_v2_fdk_48k.m4a", 0x3e4ec50f6aa27028, 1440000),
    ("aac/heaac_v2_fdk_adts.aac", 0xa3ba4984358ed1dc, 1331200),
    ("aac/heaac_v2_fdk_m4a.m4a", 0x590d525c668dce94, 1323000),
    ("aac/heaac_v2_fdk_vbr1.m4a", 0xdd6a792139665041, 1323000),
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
    ("mp3/mp3_11k_mono.mp3", 0x3846a165e9712933, 332352),
    ("mp3/mp3_16k_mono.mp3", 0xa04074b3f12ec83c, 481536),
    ("mp3/mp3_22k_mono.mp3", 0xaeafabef1172849e, 661500),
    ("mp3/mp3_24k.mp3", 0x59f1229685633f26, 720000),
    ("mp3/mp3_32k.mp3", 0xdd3869e6bb10bf08, 960000),
    ("mp3/mp3_48k.mp3", 0x356af5ee0dd1a837, 1440000),
    ("mp3/mp3_8k_mono.mp3", 0xa7f8e14d83313b61, 241344),
    ("mp3/mp3_abr192.mp3", 0x6866f7d77dd902c3, 1323000),
    ("mp3/mp3_allshort.mp3", 0x83885ac7d5ff6353, 1323000),
    ("mp3/mp3_cbr128.mp3", 0x839b18c3985f8936, 1323000),
    ("mp3/mp3_cbr320.mp3", 0xc6a4aee75f0b99aa, 1323000),
    ("mp3/mp3_cbr64_mono.mp3", 0xa7d35e672316fc5c, 1323000),
    ("mp3/mp3_copyright_orig.mp3", 0x385df40b3e455249, 1323000),
    ("mp3/mp3_crc.mp3", 0xcf7527a205215e43, 1323000),
    ("mp3/mp3_dualmono.mp3", 0xf88dc1e2c29aa035, 1323000),
    ("mp3/mp3_ffmpeg_noxing.mp3", 0x6fe1b71d1511498f, 1324800),
    ("mp3/mp3_ffmpeg_xing.mp3", 0x83885ac7d5ff6353, 1323000),
    ("mp3/mp3_force_ms.mp3", 0x99afd0dbee0f2a4c, 1323000),
    ("mp3/mp3_free_format.mp3", 0x83885ac7d5ff6353, 1323000),
    ("mp3/mp3_joint.mp3", 0x83885ac7d5ff6353, 1323000),
    ("mp3/mp3_noshort.mp3", 0x83885ac7d5ff6353, 1323000),
    ("mp3/mp3_notag_cbr.mp3", 0xfb922b03549f0525, 1324800),
    ("mp3/mp3_notag_vbr.mp3", 0xe11c524d8385622a, 1324800),
    ("mp3/mp3_preemph.mp3", 0x385df40b3e455249, 1323000),
    ("mp3/mp3_sample_albumart.mp3", 0x256fdc982722794a, 673920),
    ("mp3/mp3_sample_ascii.mp3", 0xfd371c01397247b3, 698112),
    ("mp3/mp3_sample_broken_first_frame.mp3", 0xbfcecdbc20f460e5, 186624),
    ("mp3/mp3_sample_jpg_in_mp3.mp3", 0x12b1b5ad34713ded, 9677304),
    ("mp3/mp3_sample_misidentified.mp3", 0x49fca327066ded93, 9667584),
    ("mp3/mp3_sample_misidentified2.mp3", 0x557a59ef751b7f91, 17823744),
    ("mp3/mp3_stereo.mp3", 0x76de884cae001061, 1323000),
    ("mp3/mp3_vbr_v0.mp3", 0x3ec74c85df739639, 1323000),
    ("mp3/mp3_vbr_v2.mp3", 0x2987b0b20df60b19, 1323000),
    ("mp3/mp3_vbr_v9.mp3", 0x3e50af44ec38d848, 661500),
    ("vorbis/vorbis_22k_mono.ogg", 0x0708ff11c442b0f2, 661500),
    ("vorbis/vorbis_3ch.ogg", 0xa35c5d9e9b50fb48, 1323000),
    ("vorbis/vorbis_48k.ogg", 0x5d7deed0cdca30e9, 1440000),
    ("vorbis/vorbis_5_1.ogg", 0xa97f46d71bf514de, 1323000),
    ("vorbis/vorbis_7_1.ogg", 0xb4266fabbd43c4be, 1323000),
    ("vorbis/vorbis_8k_mono.ogg", 0xd5aeba7c7f150776, 240000),
    ("vorbis/vorbis_96k_24bit.ogg", 0x47e7e9ad13a69683, 1920000),
    ("vorbis/vorbis_chained_diff.ogg", 0x171eb970df84c773, 1323000),
    ("vorbis/vorbis_chained_same.ogg", 0x171eb970df84c773, 1323000),
    ("vorbis/vorbis_ffmpeg_libvorbis.ogg", 0x5dbfce270a3380f6, 1323000),
    ("vorbis/vorbis_ffmpeg_native.ogg", 0x789c312f3ffc70e5, 1323008),
    ("vorbis/vorbis_managed_cbr.ogg", 0x85f0c74f90c39212, 1323000),
    ("vorbis/vorbis_mono.ogg", 0x5021d668aa1414cc, 1323000),
    ("vorbis/vorbis_q-1.ogg", 0xaf0f97f9b3bf5071, 1323000),
    ("vorbis/vorbis_q10.ogg", 0x8b0c9e9c3fb4adec, 1323000),
    ("vorbis/vorbis_q3.ogg", 0x642e2bd32cb7d8ac, 1323000),
    ("vorbis/vorbis_q6.ogg", 0xde00005ff797525a, 1323000),
    ("vorbis/vorbis_quad.ogg", 0xae3864ea7d56f24f, 1323000),
];

fn decode_hash(path: &Path) -> (u64, u64) {
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

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut put = |v: u32| hash = (hash ^ u64::from(v)).wrapping_mul(0x100_0000_01b3);
    let mut frames = 0u64;
    let (mut f, mut i, mut u) = (Vec::<f32>::new(), Vec::<i32>::new(), Vec::<u32>::new());

    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        let Ok(buf) = decoder.decode(&packet)
        else {
            put(0xdead);
            continue;
        };
        let n = buf.samples_interleaved();
        frames += buf.frames() as u64;
        f.resize(n, 0.0);
        i.resize(n, 0);
        u.resize(n, 0);
        buf.copy_to_slice_interleaved(&mut f);
        buf.copy_to_slice_interleaved(&mut i);
        buf.copy_to_slice_interleaved(&mut u);
        put(n as u32);
        for k in 0..n {
            put(f[k].to_bits());
            put(i[k] as u32);
            put(u[k]);
        }
    }

    (hash, frames)
}

#[test]
fn decode_output_matches_golden_hashes() {
    let Some(root) = std::env::var_os("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set, skipping");
        return;
    };

    let mut failures = Vec::new();
    for &(rel, hash, frames) in GOLDEN {
        // The FFT QMF banks of SBR are not bit exact with the pinned output.
        if cfg!(feature = "aac-sbr-fft-qmf") && rel.contains("heaac") {
            continue;
        }
        let path = Path::new(&root).join(rel);
        if !path.exists() {
            continue;
        }
        let (got_hash, got_frames) = decode_hash(&path);
        if (got_hash, got_frames) != (hash, frames) {
            failures.push(format!("{rel}: {got_hash:016x}/{got_frames} != {hash:016x}/{frames}"));
        }
    }

    assert!(failures.is_empty(), "decode output changed:\n{}", failures.join("\n"));
}
