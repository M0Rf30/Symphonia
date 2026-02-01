// Opus File Player using Symphonia
// Demonstrates full integration with OGG container and audio output

use std::env;
use std::fs::File;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia_codec_opus::OpusDecoder;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <opus_file.opus>", args[0]);
        eprintln!("\nDecodes and prints information about an Opus file in OGG container.");
        std::process::exit(1);
    }

    let path = &args[1];

    println!("Opus File Decoder");
    println!("=================");
    println!("File: {}\n", path);

    // Open the media file
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: Failed to open file: {}", e);
            std::process::exit(1);
        }
    };

    // Create a media source stream
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create a hint to help the format registry
    let mut hint = Hint::new();
    hint.with_extension("opus");

    // Probe the media source
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();

    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &format_opts,
        &metadata_opts,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: Failed to probe file: {}", e);
            std::process::exit(1);
        }
    };

    let mut format = probed.format;

    // Find the Opus track
    let track = match format.default_track() {
        Some(t) => t,
        None => {
            eprintln!("Error: No tracks found in file");
            std::process::exit(1);
        }
    };

    println!("Track Information:");
    println!("  Codec: {:?}", track.codec_params.codec);
    if let Some(rate) = track.codec_params.sample_rate {
        println!("  Sample Rate: {} Hz", rate);
    }
    if let Some(channels) = track.codec_params.channels {
        println!("  Channels: {}", channels.count());
    }
    if let Some(n_frames) = track.codec_params.n_frames {
        let duration_secs = n_frames as f64 / 48000.0;
        println!("  Duration: {:.2} seconds ({} frames)", duration_secs, n_frames);
    }
    println!();

    // Verify it's an Opus track
    if track.codec_params.codec != CODEC_TYPE_OPUS {
        eprintln!("Error: Not an Opus track");
        std::process::exit(1);
    }

    // Create decoder
    let decoder_opts = DecoderOptions::default();
    let mut decoder = match OpusDecoder::try_new(&track.codec_params, &decoder_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: Failed to create decoder: {}", e);
            std::process::exit(1);
        }
    };

    println!("✓ Decoder created successfully\n");

    // Decode packets
    let mut packet_count = 0;
    let mut sample_count: u64 = 0;
    let mut error_count = 0;

    println!("Decoding packets...");

    loop {
        // Get the next packet
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                println!("\n✓ Reached end of file");
                break;
            }
            Err(e) => {
                eprintln!("\nError reading packet: {}", e);
                error_count += 1;
                if error_count > 10 {
                    eprintln!("Too many errors, stopping");
                    break;
                }
                continue;
            }
        };

        // Decode the packet
        match decoder.decode(&packet) {
            Ok(decoded) => {
                packet_count += 1;
                sample_count += decoded.frames() as u64;

                if packet_count % 100 == 0 {
                    print!("\r  Packets: {}  Samples: {}  Errors: {}",
                           packet_count, sample_count, error_count);
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                }
            }
            Err(e) => {
                error_count += 1;
                if error_count <= 5 {
                    eprintln!("\nDecode error: {}", e);
                }
            }
        }
    }

    println!("\n\nDecoding Statistics:");
    println!("  Total packets decoded: {}", packet_count);
    println!("  Total samples: {}", sample_count);
    println!("  Duration: {:.2} seconds", sample_count as f64 / 48000.0);
    println!("  Decode errors: {}", error_count);

    if error_count == 0 {
        println!("\n✓ Decoded successfully with no errors!");
    } else {
        println!("\n⚠ Decoded with {} errors", error_count);
    }

    // Optional: Write to WAV file if requested
    if args.len() >= 3 && args[2] == "--write-wav" {
        println!("\nNote: WAV writing not implemented in this example.");
        println!("      You can use the 'symphonia-play' tool for audio output.");
    }
}
