// Simple Opus decoder test program
use std::env;
use std::fs::File;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <opus_file>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];
    println!("Decoding: {}", path);

    // Open the media source
    let file = Box::new(File::open(path)?);
    let mss = MediaSourceStream::new(file, Default::default());

    // Create a probe hint using the file extension
    let mut hint = Hint::new();
    if let Some(ext) = path.split('.').last() {
        hint.with_extension(ext);
    }

    // Probe the media source
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;

    let mut format = probed.format;

    // Find the Opus track
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec == CODEC_TYPE_OPUS)
        .ok_or("No Opus track found")?;

    println!("Track info:");
    println!("  Codec: {:?}", track.codec_params.codec);
    println!("  Sample rate: {:?}", track.codec_params.sample_rate);
    println!("  Channels: {:?}", track.codec_params.channels);
    println!("  Time base: {:?}", track.codec_params.time_base);

    // Create decoder
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())?;

    let track_id = track.id;
    let mut packet_count = 0;
    let mut sample_count = 0u64;
    let mut frame_count = 0;

    println!("\nDecoding packets...");

    // Decode loop
    loop {
        // Get the next packet
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => {
                // The decoder needs to be reset
                decoder.reset();
                continue;
            }
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // End of stream
                println!("\nEnd of stream reached");
                break;
            }
            Err(err) => {
                eprintln!("Error reading packet: {}", err);
                break;
            }
        };

        // Skip packets from other tracks
        if packet.track_id() != track_id {
            continue;
        }

        packet_count += 1;

        // Decode the packet
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                // Get sample count
                let spec = *audio_buf.spec();
                let duration = audio_buf.capacity() as u64;
                sample_count += duration;
                frame_count += 1;

                if packet_count <= 5 || packet_count % 100 == 0 {
                    println!("  Packet #{}: {} samples, {} channels at {}Hz",
                             packet_count, duration, spec.channels.count(), spec.rate);
                }
            }
            Err(Error::DecodeError(msg)) => {
                eprintln!("Decode error on packet {}: {}", packet_count, msg);
            }
            Err(err) => {
                eprintln!("Error decoding packet {}: {}", packet_count, err);
                return Err(err.into());
            }
        }
    }

    println!("\nDecoding complete!");
    println!("  Total packets decoded: {}", packet_count);
    println!("  Total frames: {}", frame_count);
    println!("  Total samples: {}", sample_count);

    if let Some(rate) = track.codec_params.sample_rate {
        let duration_secs = sample_count as f64 / rate as f64;
        println!("  Duration: {:.2} seconds", duration_secs);
    }

    Ok(())
}
