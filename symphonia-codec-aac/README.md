# Symphonia AAC Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_aac-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-aac)

Advanced Audio Coding (AAC) decoder for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Support

This decoder implements the low-complexity (LC) profile as defined in ISO/IEC 14496-3, plus
the HE-AAC v1 (Spectral Band Replication, §4.6.18) and HE-AAC v2 (Parametric Stereo, Annex
8.A) extensions, and multichannel configurations (`channelConfiguration` 1-7 and an explicit
`program_config_element()`).

HE-AAC v1/v2 are ported from the MIT-licensed `oxideav-aac` 0.1.7 crate (see `NOTICE`).
Parametric Stereo is only supported when the whole stream is a single mono `SCE` core
element (the near-universal real-world HE-AAC v2 shape); PS combined with any other channel
configuration is rejected as unsupported rather than guessed at.

AAC-LC also decodes frames of 960 samples (`frameLengthFlag` of the audio specific config, with
960 sample long and 120 sample short windows); SBR with 960 sample frames is rejected as
unsupported.

The AAC Main profile (backward adaptive prediction) and AAC LTP (long term prediction, with TNS
filtering of the predicted spectrum) decode with frames of 1024 samples, without SBR. The Main
prediction follows FFmpeg's `predict()`, which rounds the predictor state to 16 bit floats. AAC
SSR (gain control and the polyphase quadrature filter) is not supported.

USAC (audio object type 42, xHE-AAC) decodes its frequency-domain core coder with frames of 1024
or 768 samples: arithmetic coded spectra, mid/side and complex stereo prediction, TNS, noise filling and
`fac_data()` (applied when it follows an LPD frame). Pre-roll access units prime the decoder (only a decoder
without history uses them). 2:1, 8:3 and 4:1 enhanced SBR (the latter two with a 24 and 16 band analysis bank, the 8:3
ratio with a 768 sample core frame; without harmonic transposition, PVC or inter-TES) is decoded. The linear-prediction (ACELP/TCX) core is decoded for single channel elements, including
the switching between it and the frequency-domain core (transition windows, forward aliasing
cancellation and the stop-start window); a long window directly after an LPD frame is rejected.
MPEG Surround 2-1-2 (`stereoConfigIndex` 1: a single core channel, eSBR, then the upmix of the QMF
slots; phase coding, high rate mode, smoothing and decorrelation) is decoded and matches libxaac
to better than 100 dB in the QMF domain and libfdk-aac to better than 50 dB. Its temporal shaping,
residual coding and SBR after the upmix are not implemented.
Linear-prediction channel pairs, the other enhanced SBR tools and
time-warped MDCT are not implemented, and a stream or frame that needs one is rejected as
unsupported. The output matches libfdk-aac to
within 1 LSB for the FD streams tested; the LPD core output and the mixed FD/LPD core output match libxaac to better than 78 dB
(except for fixed point noise on a few short window frames)
and the final 2:1 eSBR output matches libfdk-aac to about 37 dB signal to noise ratio. A 768 sample
core stream matches libfdk-aac to 80 dB in frequency-domain frames and, like libxaac, to about
46 dB in LPD frames (a low-frequency difference whose cause is not known).

AAC-LD (audio object type 23) and AAC-ELD (audio object type 39) are decoded with frame lengths
of 512 and 480 samples, for any channel configuration of 1-7. Their low overlap window, the low
delay filterbank, and their scale factor bands and syntax (which has no element identifiers and
puts the TNS data after the gain control flag) are implemented. The error resilience tools
(Huffman codeword reordering, reversible variable length coding, and virtual codebooks), LTP in
AAC-LD, and SBR with AAC-LD are not supported, and such streams are rejected as unsupported. The
scale factor band tables of AAC-LD and AAC-ELD are those of 22.05 kHz and above of the standard:
lower sampling frequencies use those of 22.05 kHz and 24 kHz.

AAC-ELD with low delay SBR (`ldSbrPresentFlag`) is decoded with the complex-valued (high quality)
tool, at the core rate (`ldSbrSamplingRate` 0, downsampled SBR) or at twice the core rate (dual-rate
SBR), with 16 or 15 QMF time slots per frame for core frames of 512 or 480 samples. Low delay SBR
has its own filterbanks (the CLDFB, with a 320 or 640 tap non-symmetric prototype filter), a
shorter time grid syntax (a fixed grid, or one with a transient position), and no overlap with the
previous frame. Its payloads follow the channel elements of every frame, one for each single or
channel pair element, with the SBR header of the config until a payload transmits another. The
CRC of the SBR payloads is not verified. Seeking in streams with low delay SBR starts decoding
from a multiple of 32 (512 samples) or 512 (480 samples) frames from the start of the stream, so
that the noise phase of the SBR is that of a continuous decode.

## Transports

* `AdtsReader` demultiplexes ADTS (one raw data block per frame).
* `LoasReader` demultiplexes LATM with LOAS framing (a single program and layer).
* `AdifReader` demultiplexes ADIF (a single program). Raw data blocks of an ADIF stream have no
  frame headers: the reader finds the end of each block by parsing its syntax, up to its `ID_END`
  element, so it costs as much as parsing (but not synthesising) the audio, and a stream that ends
  with other data ends at its last block. The duration is estimated from the first blocks. Seeking
  is by block: positions of blocks are remembered as they are parsed, so a first seek into a long
  stream parses up to the target.

HE-AAC in ADTS, ADIF, and LATM with a plain AAC-LC config does not signal SBR or Parametric
Stereo. The readers look for an `EXT_SBR_DATA` payload in the first blocks of the stream. If they
find one, the codec parameters report the decoder output (twice the core rate, stereo for
Parametric Stereo), the audio specific config of the parameters signals the extension, and the
timeline (packet timestamps and durations, duration, seek positions) is in decoded frames, as it is
for MP4. SBR can only be found in streams of a core rate of at most 32 kHz, which are
AAC-LC with a predefined channel configuration or a program config element.

## Attribution

Symphonia's AAC decoder was ported and relicensed from the [NihAV](https://nihav.org/) project with permission from the original author, Kostya Shishkov. The first commit with the original decoder is `3aeeb22`.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
