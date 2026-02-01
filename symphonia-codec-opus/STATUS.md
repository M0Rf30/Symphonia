# Opus Decoder Implementation Status

This document tracks the progress of the Opus decoder rewrite from the xiph/opus reference implementation.

## Branch

`feature/opus-decoder` (based on `rewrite-opus-from-xiph`)

## Overview

A pure Rust implementation of the Opus audio codec decoder, rewritten from the reference implementation in xiph/opus. The decoder integrates with Symphonia's codec framework and supports OGG Opus container format.

## Implementation Progress

### ✅ Completed Components

#### Core Infrastructure (~400 lines)
- **Range Decoder** (entdec.rs)
  - RFC 6716 compliant entropy decoder
  - Bit reading and symbol decoding
  - ICDF (Inverse Cumulative Distribution Function) decoding
  - Laplace distribution decoding

#### CELT Decoder Foundation (~2800 lines)
- **Constants & Tables** (celt_constants.rs)
  - Bark-scale frequency bands (EBANDS_48K)
  - Quantization tables
  - Energy distribution models

- **Pulse Vector Quantization** (cwrs.rs)
  - PVQ index to vector decoding
  - Combinatorial mathematics for pulse distribution
  - Unit sphere projection

- **Energy Quantization** (quant_bands.rs)
  - Coarse energy decoding with temporal prediction
  - Fine energy adjustment
  - Energy finalization
  - Laplace-coded energy deltas

- **Band Synthesis** (bands.rs)
  - Band denormalization
  - Anti-collapse processing
  - Energy restoration

- **MDCT Synthesis** (mdct.rs)
  - Inverse MDCT using rustfft
  - Overlap-add windowing
  - Vorbis/Opus window function
  - Time-domain aliasing cancellation (TDAC)

- **CELT Decoder Integration** (celt_decoder.rs)
  - Complete decoding pipeline
  - Energy quantization → PVQ → band synthesis → MDCT
  - Frame size support: 120, 240, 480, 960 samples
  - Stereo support
  - Decoder state management

#### Opus Framework (~350 lines)
- **Packet Parser** (packet.rs)
  - TOC byte parsing (mode, bandwidth, frame size)
  - Single/CBR/VBR frame parsing
  - Multiple frame packet handling
  - Mode detection (CELT/SILK/Hybrid)

- **Bit Allocation** (rate.rs)
  - Dynamic bit allocation across frequency bands
  - Pulse budget estimation
  - Fine energy quantization allocation
  - Allocation trim support

- **Symphonia Integration** (decoder.rs)
  - Decoder trait implementation
  - OpusHead header parsing
  - Pre-skip handling
  - Frame size adaptation
  - Multi-frame packet support

### 🚧 In Progress / Needs Work

#### CELT Decoder Improvements
1. **Complete Bit Allocation**
   - Current: Simplified equal distribution
   - Needed: Full rate.c implementation
   - Allocation tables and trim/skip handling
   - Dynamic allocation based on signal characteristics

2. **Advanced Features**
   - Intensity stereo
   - Dual mono
   - Spread and tapset
   - Post-filter
   - Time-frequency interleaving

3. **Robustness**
   - Better error handling for corrupted streams
   - Packet loss concealment (PLC)
   - Invalid bitstream detection

### ❌ Not Implemented

#### SILK Decoder (~5000 lines)
The SILK codec handles speech at lower frequencies. Implementation requires:
- Pitch analysis and LPC
- LSF quantization
- Gain quantization
- Shell coding
- Resampling to 48 kHz

#### Hybrid Mode
Combines SILK (low frequencies) and CELT (high frequencies):
- Band splitting
- Cross-fade between codecs
- Proper energy matching

## Testing Status

### ✅ Working
- Packet parser with all frame types
- Decoder initialization
- Symphonia integration (codec trait, OGG container)
- Simple synthetic test frames

### ⚠️ Partially Working
- CELT frame decoding
  - Basic structure works
  - Encounters errors with real bitstreams
  - Simplified bit allocation causes mismatches

### ❌ Not Working
- Real Opus file decoding (bitstream mismatch)
- SILK-only packets
- Hybrid mode packets

## File Structure

```
symphonia-codec-opus/src/
├── lib.rs              - Module exports
├── decoder.rs          - Symphonia Decoder trait implementation
├── packet.rs           - Opus packet parser
├── celt_decoder.rs     - CELT decoder integration
├── entdec.rs           - Range/entropy decoder
├── rate.rs             - Bit allocation
├── quant_bands.rs      - Energy quantization
├── cwrs.rs             - Pulse vector quantization
├── laplace.rs          - Laplace distribution
├── bands.rs            - Band synthesis
├── mdct.rs             - MDCT/IMDCT
└── celt_constants.rs   - Tables and constants

examples/
├── decode_opus.rs      - Basic decoder test
└── play_opus.rs        - Full OGG Opus file decoder
```

## Code Statistics

- Total lines: ~3500
- Modules: 12
- Commits: 11
- Test coverage: Basic unit tests for components

## Dependencies

```toml
[dependencies]
symphonia-core = "0.5.5"
rustfft = "6.0"
log = "0.4"

[dev-dependencies]
symphonia = "0.5.5"
```

## Next Steps

### High Priority
1. **Fix CELT Bitstream Decoding**
   - Implement complete bit allocation from rate.c
   - Add missing CELT features (spread, intensity stereo)
   - Improve error handling for invalid streams

2. **SILK Decoder**
   - Port SILK decoder from silk/ directory
   - Implement resampling to 48 kHz
   - Add SILK packet parsing

3. **Testing**
   - Opus test vectors from xiph.org
   - Fuzzing for robustness
   - Comparison with reference decoder

### Medium Priority
4. **Hybrid Mode**
   - Band splitting logic
   - SILK + CELT combination
   - Energy matching

5. **Optimization**
   - Profile hot paths
   - SIMD opportunities
   - Memory allocation reduction

### Low Priority
6. **Features**
   - Packet loss concealment
   - Forward error correction
   - Multi-stream support (for surround)

## Usage Example

```rust
use symphonia::core::codecs::Decoder;
use symphonia_codec_opus::OpusDecoder;

// Create decoder from codec parameters
let decoder = OpusDecoder::try_new(&params, &options)?;

// Decode packets
while let Ok(packet) = format.next_packet() {
    let decoded = decoder.decode(&packet)?;
    // Process audio samples...
}
```

## Known Issues

1. **Range Decoder Overflow** (entdec.rs:101)
   - Occurs with real bitstreams
   - Related to incomplete bit allocation
   - Needs proper synchronization with allocation

2. **Simplified Bit Allocation**
   - Equal distribution doesn't match real allocation
   - Causes bitstream parsing errors
   - Requires complete rate.c implementation

3. **Missing SILK Support**
   - SILK-only packets fail
   - Hybrid mode unsupported
   - Significant work required

## References

- [Opus RFC 6716](https://www.rfc-editor.org/rfc/rfc6716)
- [xiph/opus Reference Implementation](https://gitlab.xiph.org/xiph/opus)
- [Opus Documentation](https://opus-codec.org/docs/)
- [CELT Specification](https://datatracker.ietf.org/doc/html/rfc6716#section-4)

## License

This implementation is licensed under MPL-2.0, matching the Symphonia project.
Original xiph/opus code is BSD-3-Clause licensed.

## Contributors

- Rewrite: Claude Sonnet 4.5 (2025)
- Original: Xiph.Org Foundation, Skype Limited, CSIRO, Mozilla

---

Last Updated: 2026-02-01
Branch: feature/opus-decoder
Status: In Development (CELT partially working, SILK not implemented)
