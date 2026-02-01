// Opus Diagnostic Tool - Analyze decoder output
// Helps identify decoding issues

use std::env;
use std::fs::File;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use symphonia_codec_opus::OpusDecoder;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <opus_file>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];

    println!("Opus Decoder Diagnostics");
    println!("========================\n");

    // Open file
    let file = File::open(path).expect("Failed to open file");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.split('.').last() {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .expect("Failed to probe");

    let mut format = probed.format;
    let track = format.default_track().expect("No track found");

    if track.codec_params.codec != CODEC_TYPE_OPUS {
        eprintln!("Not an Opus file");
        std::process::exit(1);
    }

    let sample_rate = track.codec_params.sample_rate.unwrap_or(48000);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    println!("File: {}", path);
    println!("Sample Rate: {} Hz", sample_rate);
    println!("Channels: {}\n", channels);

    // Create decoder
    let mut decoder: Box<dyn Decoder> = Box::new(
        OpusDecoder::try_new(&track.codec_params, &DecoderOptions::default())
            .expect("Failed to create decoder")
    );

    println!("Analyzing first 10 packets...\n");

    for i in 0..10 {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(e) => {
                println!("Packet {}: Error reading - {}", i, e);
                break;
            }
        };

        match decoder.decode(&packet) {
            Ok(decoded) => {
                analyze_audio_buffer(&decoded, i);
            }
            Err(e) => {
                println!("Packet {}: Decode error - {}", i, e);
            }
        }
    }
}

fn analyze_audio_buffer(audio: &AudioBufferRef, packet_num: usize) {
    match audio {
        AudioBufferRef::F32(buf) => {
            let channels = buf.spec().channels.count();
            let frames = buf.frames();

            println!("Packet #{}", packet_num);
            println!("  Frames: {}", frames);
            println!("  Channels: {}", channels);

            // Analyze each channel
            for ch in 0..channels {
                let samples = buf.chan(ch);

                // Calculate statistics
                let mut min = f32::MAX;
                let mut max = f32::MIN;
                let mut sum = 0.0f64;
                let mut sum_sq = 0.0f64;
                let mut zero_count = 0;
                let mut nan_count = 0;
                let mut inf_count = 0;

                for &sample in samples.iter().take(frames) {
                    if sample.is_nan() {
                        nan_count += 1;
                    } else if sample.is_infinite() {
                        inf_count += 1;
                    } else {
                        min = min.min(sample);
                        max = max.max(sample);
                        sum += sample as f64;
                        sum_sq += (sample as f64) * (sample as f64);
                        if sample.abs() < 1e-10 {
                            zero_count += 1;
                        }
                    }
                }

                let mean = sum / frames as f64;
                let variance = (sum_sq / frames as f64) - (mean * mean);
                let rms = (sum_sq / frames as f64).sqrt();

                println!("  Channel {}:", ch);
                println!("    Range: [{:.6}, {:.6}]", min, max);
                println!("    Mean: {:.6}", mean);
                println!("    RMS: {:.6}", rms);
                println!("    Variance: {:.6}", variance);
                println!("    Near-zero samples: {}/{}", zero_count, frames);

                if nan_count > 0 {
                    println!("    ⚠ WARNING: {} NaN samples!", nan_count);
                }
                if inf_count > 0 {
                    println!("    ⚠ WARNING: {} Inf samples!", inf_count);
                }

                // Show first 10 samples
                print!("    First 10 samples: [");
                for i in 0..10.min(frames) {
                    print!("{:.6}", samples[i]);
                    if i < 9 && i < frames - 1 {
                        print!(", ");
                    }
                }
                println!("]");

                // Check if all samples are the same (indicates a problem)
                if max - min < 1e-10 {
                    println!("    ⚠ WARNING: All samples are nearly identical!");
                }

                // Check if RMS is too high (clipping/overflow)
                if rms > 1.0 {
                    println!("    ⚠ WARNING: RMS > 1.0 - possible clipping!");
                }

                // Check if RMS is too low (silence or gain issue)
                if rms < 0.0001 {
                    println!("    ⚠ WARNING: RMS < 0.0001 - very quiet or silent!");
                }
            }
            println!();
        }
        _ => {
            println!("Packet #{}: Unsupported buffer format", packet_num);
        }
    }
}
