// Inspect PCM output from DSD decoder
use std::env;
use std::fs::File;

use symphonia_codec_dsd::DsdDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <file.dsf>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];
    let file = File::open(path).expect("Failed to open file");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create format reader directly (DSF).
    let reader_result = symphonia_format_dsd::DsfReader::try_new(mss, FormatOptions::default());
    let mut reader: Box<dyn FormatReader> = Box::new(reader_result.expect("Failed to open DSF file"));

    // Find the first audio track and clone its parameters.
    let (track_id, audio_params) = reader
        .tracks()
        .iter()
        .find_map(|t| match t.codec_params.as_ref() {
            Some(CodecParameters::Audio(p)) => Some((t.id, p.clone())),
            _ => None,
        })
        .expect("No audio track found");

    let input_rate = audio_params.sample_rate.expect("No sample rate");
    let channels = audio_params.channels.clone().expect("No channels");

    println!("Input: {} Hz, {} channels", input_rate, channels.count());

    // Enable PCM conversion at 352.8kHz.
    let output_rate = 352800u32;
    let mut params_with_pcm = audio_params.clone();
    params_with_pcm.extra_data = Some(output_rate.to_le_bytes().to_vec().into_boxed_slice());

    println!("Enabling DSD->PCM conversion: {} -> {} Hz", input_rate, output_rate);

    // Create the DSD decoder directly.
    let mut decoder = DsdDecoder::try_new(&params_with_pcm, &AudioDecoderOptions::default())
        .expect("Failed to create decoder");

    let output_rate = decoder.codec_params().sample_rate.expect("No output rate");
    println!("Decoder output rate: {} Hz", output_rate);

    let mut packet_count = 0;
    let max_packets = 5;

    while packet_count < max_packets {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode_ref(&packet.as_packet_ref()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Decode error: {}", e);
                continue;
            }
        };

        packet_count += 1;

        match decoded {
            GenericAudioBufferRef::F32(buf) => {
                let frames = buf.frames();
                let spec = buf.spec();

                println!("\n=== Packet {} ===", packet_count);
                println!("Frames: {}", frames);
                println!("Channels: {}", spec.channels().count());
                println!("Sample rate: {} Hz", spec.rate());

                // Get samples from the first channel.
                let ch0 = buf.plane(0).expect("channel 0");

                // Statistics.
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                let mut sum = 0.0f64;
                let mut nan_count = 0;
                let mut inf_count = 0;

                for &sample in ch0 {
                    if sample.is_nan() {
                        nan_count += 1;
                    }
                    else if sample.is_infinite() {
                        inf_count += 1;
                    }
                    else {
                        min = min.min(sample);
                        max = max.max(sample);
                        sum += sample as f64;
                    }
                }

                let avg = if frames > 0 { sum / frames as f64 } else { 0.0 };

                println!("Channel 0 samples:");
                println!("  Min: {}", min);
                println!("  Max: {}", max);
                println!("  Avg: {}", avg);
                println!("  NaN count: {}", nan_count);
                println!("  Inf count: {}", inf_count);

                // Show the first 20 samples.
                println!("  First 20 samples: {:?}", &ch0[..20.min(ch0.len())]);

                // Check for silence or constant DC.
                if max.abs() < 0.001 && min.abs() < 0.001 {
                    println!("  WARNING: Output is near silence!");
                }
                if (max - min).abs() < 0.001 {
                    println!("  WARNING: Output is constant DC!");
                }
            }
            GenericAudioBufferRef::U8(_) => {
                println!("ERROR: Got U8 buffer (expected F32)");
            }
            _ => {
                println!("ERROR: Got unexpected buffer type");
            }
        }
    }

    println!("\n=== Summary ===");
    println!("Processed {} packets", packet_count);
}
