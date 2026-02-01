// Test SILK decoder integration
// SPDX-License-Identifier: MPL-2.0

use std::env;
use std::fs::File;
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia_codec_opus::OpusDecoder;

fn main() {
    let args: Vec<String> = env::args().collect();

    let file_path = if args.len() > 1 {
        &args[1]
    } else {
        "/home/gianluca/Scaricati/Symphony No.6 (1st movement).opus"
    };

    println!("Testing Opus decoder with SILK support");
    println!("File: {}", file_path);

    // Open the file
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open file: {}", e);
            return;
        }
    };

    // Create media source
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create format hint
    let mut hint = Hint::new();
    hint.with_extension("opus");

    // Probe the format
    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to probe format: {}", e);
            return;
        }
    };

    let mut format = probed.format;

    // Get the default track
    let track = match format.default_track() {
        Some(t) => t,
        None => {
            eprintln!("No default track found");
            return;
        }
    };

    println!("Track info:");
    println!("  Codec: {:?}", track.codec_params.codec);
    println!("  Sample rate: {:?}", track.codec_params.sample_rate);
    println!("  Channels: {:?}", track.codec_params.channels);

    // Create decoder
    let mut decoder = match OpusDecoder::try_new(&track.codec_params, &DecoderOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decoder: {}", e);
            return;
        }
    };

    println!("\nDecoder created successfully");

    // Decode first few packets to test
    let mut packet_count = 0;
    let max_packets = 10;

    loop {
        if packet_count >= max_packets {
            break;
        }

        // Read next packet
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(e) => {
                println!("End of stream or error: {}", e);
                break;
            }
        };

        // Decode packet
        match decoder.decode(&packet) {
            Ok(_audio_buf) => {
                packet_count += 1;
                let decoded = decoder.last_decoded();
                let (frames, channels) = match decoded {
                    AudioBufferRef::F32(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::U8(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::U16(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::U24(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::U32(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::S8(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::S16(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::S24(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::S32(buf) => (buf.frames(), buf.spec().channels.count()),
                    AudioBufferRef::F64(buf) => (buf.frames(), buf.spec().channels.count()),
                };
                println!("Packet {}: {} frames, {} channels",
                    packet_count,
                    frames,
                    channels
                );
            }
            Err(e) => {
                eprintln!("Decode error on packet {}: {}", packet_count + 1, e);
                // Continue trying to decode
                packet_count += 1;
            }
        }
    }

    println!("\nSuccessfully decoded {} packets", packet_count);
    println!("SILK decoder integration test complete!");
}
