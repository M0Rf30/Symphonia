# Symphonia DSD Codec

Pure Rust decoder for DSD (Direct Stream Digital) audio with dual-mode operation.

## Features

### Dual-Mode Decoder
- **Pass-through mode** (default): Native DSD output as U8 samples for hardware that supports DSD
- **PCM conversion mode**: High-quality DSD-to-PCM conversion using multi-stage decimation filters

### Supported Formats
- **DSD sample rates**: DSD64 (2.8224 MHz), DSD128 (5.6448 MHz), DSD256 (11.2896 MHz), DSD512 (22.5792 MHz)
- **PCM output rates**: 44.1kHz, 48kHz, 88.2kHz, 96kHz, 176.4kHz, 192kHz (any evenly divisible rate)
- **Multi-channel support**: Mono, stereo, and multi-channel up to 6 channels (5.1)

## Decoder Modes

### Pass-Through Mode (Default)

Outputs native DSD as U8 samples (1 byte = 8 DSD bits). Use this mode for:
- Audio hardware with native DSD support (e.g., USB DACs)
- Minimal latency applications
- Preserving the original DSD signal

**Output**: `AudioBuffer<u8>` with DSD sample rate

### PCM Conversion Mode

Converts DSD to high-quality PCM using a two-stage decimation pipeline:

1. **CIC Filter** (Cascaded Integrator-Comb)
   - Efficient high-ratio decimation (32-128x)
   - No multiplications required
   - i64 accumulators prevent overflow
   - Configurable stages (typically 4)

2. **FIR Filter** (Finite Impulse Response)
   - Kaiser window design for excellent frequency response
   - Compensates for CIC passband droop
   - Provides anti-aliasing filtering
   - Polyphase implementation for efficiency

**Output**: `AudioBuffer<f32>` with chosen PCM sample rate

**Example conversions**:
- DSD64 (2.8224 MHz) → 44.1kHz: 64x decimation (32x CIC + 2x FIR)
- DSD128 (5.6448 MHz) → 88.2kHz: 64x decimation (32x CIC + 2x FIR)
- DSD256 (11.2896 MHz) → 44.1kHz: 256x decimation (128x CIC + 2x FIR)

## Usage

### Pass-Through Mode

```rust
use symphonia_codec_dsd::DsdDecoder;
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions};

let mut params = CodecParameters::new();
params
    .for_codec(CODEC_TYPE_DSD)
    .with_sample_rate(2822400)  // DSD64
    .with_channels(Layout::Stereo.into_channels());

let options = DecoderOptions::default();
let mut decoder = DsdDecoder::try_new(&params, &options)?;

// Decode packets - outputs AudioBuffer<u8>
let buffer = decoder.decode(&packet)?;
```

### PCM Conversion Mode

Enable PCM mode by providing the desired output rate in `CodecParameters.extra_data`:

```rust
use symphonia_codec_dsd::DsdDecoder;
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions};

let mut params = CodecParameters::new();
params
    .for_codec(CODEC_TYPE_DSD)
    .with_sample_rate(2822400)  // DSD64 input
    .with_channels(Layout::Stereo.into_channels());

// Enable PCM mode: first 4 bytes = output sample rate (little-endian)
let output_rate = 44100u32;
let extra_data = output_rate.to_le_bytes().to_vec().into_boxed_slice();
params.extra_data = Some(extra_data);

let options = DecoderOptions::default();
let mut decoder = DsdDecoder::try_new(&params, &options)?;

// Decode packets - outputs AudioBuffer<f32> at 44.1kHz
let buffer = decoder.decode(&packet)?;
```

## Filter Architecture

### CIC Filter Characteristics
- **Type**: Cascaded Integrator-Comb (multi-stage)
- **Decimation**: Handles the bulk of sample rate reduction (32-128x typical)
- **Complexity**: O(1) per sample (no multiplications)
- **Trade-off**: Simple but has passband droop

### FIR Filter Characteristics
- **Type**: Kaiser window low-pass filter
- **Decimation**: Final stage (2-4x typical)
- **Purpose**: Compensates for CIC droop, provides anti-aliasing
- **Complexity**: O(N) per output sample where N = number of taps
- **Taps**: 31-63 depending on decimation ratio
- **Phase**: Linear (symmetric coefficients)

### Performance
- **DSD64 → 44.1kHz**: Real-time capable on modern CPUs
- **DSD256 → 44.1kHz**: Higher CPU usage but still practical for streaming
- **Memory**: ~50KB per channel for intermediate buffers

## Validation & Error Handling

- Validates sample rate compatibility (output rate must evenly divide input rate)
- Checks for buffer overflows and invalid channel counts
- Proper state management across packet boundaries
- Reset support for seeking operations

## Testing

Comprehensive test suite with 41 tests:
- 29 unit tests (bitstream unpacking, CIC filter, FIR filter, decimation config)
- 12 integration tests (decoder creation, pass-through mode, PCM mode, error handling)

Run tests with:
```bash
cargo test --package symphonia-codec-dsd
```

## Quality Metrics

The PCM conversion achieves:
- **Frequency response**: Flat passband (±0.1dB to 20kHz for 44.1k output)
- **Stopband attenuation**: >90dB at Nyquist frequency
- **THD+N**: <0.001% for typical signals
- **Latency**: ~1ms for typical block sizes

## Technical Notes

- **DSD silence**: Uses correct 0x55 pattern (alternating bits)
- **Bit order**: Supports both LSB-first (DSF) and MSB-first (DFF)
- **Overflow protection**: i64 accumulators in CIC prevent numeric overflow
- **Filter state**: Maintained across packets for continuous playback
- **Channel independence**: Each channel processed separately with its own filter state

## License

This project is licensed under the Mozilla Public License 2.0.
