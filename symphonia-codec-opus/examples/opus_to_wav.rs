// Opus to WAV converter - decode Opus files and save as WAV
// This works without requiring audio output devices

use std::env;
use std::fs::File;
use std::io::{Write, BufWriter};

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
        eprintln!("Usage: {} <input.opus> [output.wav]", args[0]);
        eprintln!("\nDecodes an Opus file and saves it as a WAV file.");
        eprintln!("If output filename is not specified, uses 'output.wav'");
        std::process::exit(1);
    }

    let input_path = &args[1];
    let output_path = if args.len() >= 3 {
        &args[2]
    } else {
        "output.wav"
    };

    println!("Opus to WAV Converter");
    println!("=====================");
    println!("Input:  {}", input_path);
    println!("Output: {}\n", output_path);

    // Open input file
    let file = match File::open(input_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("✗ Failed to open input file: {}", e);
            std::process::exit(1);
        }
    };

    // Create media source
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create hint
    let mut hint = Hint::new();
    if let Some(ext) = input_path.split('.').last() {
        hint.with_extension(ext);
    }

    // Probe the file
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();

    let probed = match symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ Failed to probe file: {}", e);
            std::process::exit(1);
        }
    };

    let mut format = probed.format;

    // Get default track
    let track = match format.default_track() {
        Some(t) => t,
        None => {
            eprintln!("✗ No tracks found");
            std::process::exit(1);
        }
    };

    // Verify Opus codec
    if track.codec_params.codec != CODEC_TYPE_OPUS {
        eprintln!("✗ Not an Opus codec");
        std::process::exit(1);
    }

    let sample_rate = track.codec_params.sample_rate.unwrap_or(48000);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    println!("Track Information:");
    println!("  Codec: Opus");
    println!("  Sample Rate: {} Hz", sample_rate);
    println!("  Channels: {}", channels);
    if let Some(n_frames) = track.codec_params.n_frames {
        println!("  Duration: {:.2} seconds", n_frames as f64 / sample_rate as f64);
    }
    println!();

    // Create decoder
    let dec_opts = DecoderOptions::default();
    let mut decoder: Box<dyn Decoder> = match OpusDecoder::try_new(&track.codec_params, &dec_opts) {
        Ok(d) => Box::new(d),
        Err(e) => {
            eprintln!("✗ Failed to create decoder: {}", e);
            std::process::exit(1);
        }
    };

    println!("✓ Decoder created");

    // Create output WAV file
    let out_file = match File::create(output_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("✗ Failed to create output file: {}", e);
            std::process::exit(1);
        }
    };

    let mut writer = BufWriter::new(out_file);

    // Write placeholder WAV header (will update later)
    let mut wav_header = vec![0u8; 44];
    writer.write_all(&wav_header).unwrap();

    println!("✓ Output file created\n");
    println!("Decoding...\n");

    // Decode packets
    let mut packet_count = 0;
    let mut total_samples = 0;
    let mut decode_errors = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                eprintln!("\n✗ Error reading packet: {}", e);
                decode_errors += 1;
                continue;
            }
        };

        match decoder.decode(&packet) {
            Ok(decoded) => {
                // Convert to 16-bit PCM and write
                let samples = convert_to_i16(&decoded, channels);

                // Skip empty packets (pre-skip or invalid)
                if samples.is_empty() {
                    continue;
                }

                total_samples += samples.len() / channels;

                // Write interleaved samples as little-endian
                for &sample in &samples {
                    writer.write_all(&sample.to_le_bytes()).unwrap();
                }

                packet_count += 1;
                if packet_count % 100 == 0 {
                    print!("\r  Packets: {}  Samples: {}  ", packet_count, total_samples);
                    std::io::Write::flush(&mut std::io::stdout()).unwrap();
                }
            }
            Err(e) => {
                eprintln!("\n✗ Decode error: {}", e);
                decode_errors += 1;
            }
        }
    }

    println!("\n\n✓ Decoding complete");
    println!("  Total packets: {}", packet_count);
    println!("  Total samples: {}", total_samples);
    println!("  Decode errors: {}", decode_errors);

    // Update WAV header with correct sizes
    let data_size = (total_samples * channels * 2) as u32;
    let file_size = data_size + 36;

    write_wav_header(&mut wav_header, sample_rate, channels as u16, data_size);

    // Seek back and write correct header
    drop(writer);
    let mut out_file = std::fs::OpenOptions::new()
        .write(true)
        .open(output_path)
        .unwrap();
    out_file.write_all(&wav_header).unwrap();

    println!("\n✓ WAV file written: {}", output_path);
    println!("  File size: {} bytes", file_size + 8);
    println!("\nYou can now play the WAV file with any audio player:");
    println!("  aplay {}", output_path);
    println!("  ffplay {}", output_path);
    println!("  mpv {}", output_path);
}

fn write_wav_header(header: &mut [u8], sample_rate: u32, channels: u16, data_size: u32) {
    let file_size = data_size + 36;

    // RIFF header
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&file_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");

    // fmt chunk
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes()); // chunk size
    header[20..22].copy_from_slice(&1u16.to_le_bytes());  // audio format (PCM)
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * channels as u32 * 2;
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    let block_align = channels * 2;
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
}

fn convert_to_i16(audio: &AudioBufferRef, channels: usize) -> Vec<i16> {
    match audio {
        AudioBufferRef::F32(buf) => {
            let frames = buf.frames();
            let mut samples = Vec::with_capacity(frames * channels);

            // Interleave channels
            for i in 0..frames {
                for ch in 0..channels {
                    let sample = buf.chan(ch)[i];
                    // Clamp and convert to 16-bit
                    let sample_i16 = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    samples.push(sample_i16);
                }
            }
            samples
        }
        _ => {
            eprintln!("✗ Unsupported audio buffer format");
            Vec::new()
        }
    }
}
