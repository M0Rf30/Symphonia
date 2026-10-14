# Symphonia DSD Codec

Pure Rust decoder for DSD (Direct Stream Digital) audio, and for DST (Direct Stream Transfer, the
lossless compression of SACD and compressed DSDIFF).

## Modes

### Pass-through (default)

Native DSD output as `U8` samples (1 byte = 8 consecutive 1-bit samples of a channel). A frame of
the output buffer is one byte per channel and the buffer's rate is `dsd_rate / 8`, so
`frames() / rate()` is the duration. The codec parameters keep the 1-bit sample rate.

### PCM

Set `extra_data` of the codec parameters to the PCM rate as a little-endian `u32`; the output is
`f32`. The DSD rate divided by the PCM rate must be an even integer of at least 8, e.g. DSD64
(2.8224 MHz) to 44.1, 88.2, 176.4 or 352.8 kHz, but not to 48 kHz (and 3.072 MHz DSD to 48 kHz).
Anything else is rejected when the decoder is created.

The decimation chain (module `decimator`) is

1. a CIC filter, exact in integer arithmetic on the packed bits (a nibble lookup table), decimating
   by `R / 2^h`, with 5 to 8 stages;
2. `h` (at most 3) FIR stages that halve the rate: Kaiser half-band filters, and a last
   Kaiser-windowed low-pass that also compensates the droop of the CIC filter.

All filters are linear phase. Measured with second order 1-bit delta-sigma encoded tones
(amplitude 0.25) through the decoder:

| output rate | passband (up to 20 kHz) | worst alias into the passband |
|-------------|-------------------------|-------------------------------|
| 44.1 kHz    | +-0.001 dB              | -104 dB                       |
| 88.2 kHz    | +-0.001 dB              | -104 dB                       |
| 176.4 kHz   | +-0.001 dB              | -105 dB                       |
| 352.8 kHz   | +-0.001 dB              | -81 dB                        |

The in-band signal-to-noise ratio of a 1 kHz tone is 96.7 dB at all four rates (97.5 dB for an
ideal filter). The 44.1 kHz filters have 19, 27 and 153 taps.

#### Delay and flushing

The filters delay the signal by about 1 ms (43 frames at 44.1 kHz). With the decoder option
`gapless` (the default) the delay is dropped from the start of the stream, so that frame `k` is the
audio at DSD sample `k * R`; the last frames are held back, and `DsdDecoder::flush()` returns them
after the last packet. A decoder that does not call `flush()` loses the last millisecond. After
`reset()` (a seek) the filters restart with no history, which gives a transient of about a
millisecond.

## DST

Streams of the codec `CODEC_ID_DST` are decompressed (module `dst`, an implementation of
ISO/IEC 14496-3 Subpart 10: segmented filters and probability tables, mappings, Rice coded
tables, half-probability start, arithmetic decoding, uncompressed frames) and then handled like DSD.
The packet layout is described in the docs of `DsdDecoder`. With the decoder option `verify` the
CRC of the decoded frame (the `DSTC` chunk) and the end of the arithmetic code are checked, and
`finalize()` reports the result. Up to 6 channels: that is the limit of the format (the
frame header and channel maps of ISO/IEC 14496-3 Subpart 10, `DST_MAX_CHANNELS` in FFmpeg).

## Testing

`cargo test -p symphonia-codec-dsd`: unit tests of the CIC (against an integrator/comb
implementation), FIR, decimator (response, alias, delay, chunking) and DST modules, decoder
tests, and, with `RMPD_SAMPLES` set, hashes of the PCM output of real DSF and DFF files and the
decoding of a DST file to the DSD of the MPEG-4 reference decoder.

DST output was also compared to that reference decoder (`dstcnv`) on DSDIFF files compressed by
a third party encoder (mono, stereo and 6 channel DSD64, prediction orders 16 and 128): identical,
and the CRCs of all 1200 frames match. The arithmetic decoder rounding is the one of FFmpeg's
`dstdec.c`, and the DSD of `dst-64fs44-2ch.dff` (wrapped as uncompressed DSDIFF) gives, through
FFmpeg, the same PCM as FFmpeg's own DST decoder, byte for byte. DSD128 and DSD256 files decode to the source.

## License

Mozilla Public License 2.0.
