# Symphonia WavPack Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_wavpack-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-wavpack)

WavPack demuxer and lossless decoder for Project Symphonia.

Supports the WavPack v1-v3 RIFF/WAVE-wrapped stream and the native WavPack v4/v5 block
stream: lossless PCM (8/16/24/32-bit), lossless and hybrid-lossy IEEE 32-bit float,
hybrid-lossy PCM, hybrid-lossless PCM and float with the `.wvc` correction file, and
multichannel files (mono/stereo/quad/5.1/7.1/... — anything expressible as a
WAVEFORMATEXTENSIBLE speaker mask, or discrete channels otherwise).

### Multichannel (>2 channel) files

A WavPack file with more than 2 channels stores its audio as several interleaved
mono/stereo "streams" (e.g. 5.1 is 3 streams: a stereo front pair, a mono center+LFE...
depending on the encoder's channel grouping), all sharing the same starting sample
index. The reader merges each such group of blocks (`INITIAL_BLOCK` .. `FINAL_BLOCK`)
into one packet, and the decoder interleaves every stream's samples into the final
N-channel output — the same approach WavPack's own `unpack_samples_interleave()` uses.
The channel count and WAVEFORMATEXTENSIBLE speaker mask come from the file's
`ID_CHANNEL_INFO` metadata; a mask that doesn't cleanly match the channel count (e.g. a
file with "unassigned" channels) falls back to `Channels::Discrete`.

### Hybrid (`-b<n>`) streams and `.wvc` correction files

WavPack's hybrid mode splits a stream into a "lossy" main bitstream (`.wv`) plus an optional
`.wvc` correction file that refines it back to bit-exact lossless. Without the correction
file a hybrid `.wv` decodes to its lossy approximation (bit-exact vs. `wvunpack -i`); with
it, the decoder reconstructs the lossless audio exactly like `wvunpack` does: the correction
bitstream narrows every sample down to its exact value, the noise-shaping state is restored
from the correction block, and the result is verified against the lossless CRC of the
correction block (and the CRC of the extension bits for 32-bit float / integer data).

Attach the correction stream in any of these ways:

* `WavPackReader::try_new_with_correction(wv_mss, wvc_mss, opts)`: both streams explicitly;
  an unusable correction stream is an error.
* `FormatOptions::sidecar(Box<dyn MediaSource>)`: a side channel through the regular probe
  (`Probe::probe`) or `WavPackReader::try_new`; a correction stream that cannot be used is
  ignored and the lossy audio is decoded.
* For applications that open files by path,
  `symphonia_codec_wavpack::with_sibling_correction(path, opts)` adds the `.wvc` file next
  to the `.wv` file (if there is one) to the options, and
  `symphonia_codec_wavpack::correction_path(path)` returns its path.

Block by block, a correction block is matched to its main block by sample position (as in
the reference library); a block without a matching or intact correction block (missing,
corrupt, truncated, or from another file) is decoded lossy, the others stay lossless.
`WavPackReader::has_correction()` reports whether a correction stream is attached. Seeking
needs both streams to be seekable.

### Integrity checks and MD5

The reader hands raw blocks to the decoder, which checks every block's checksum
(`ID_BLOCK_CHECKSUM`, as `WavpackVerifySingleBlock()`) and the CRC of the decoded samples. A
failure is a `DecodeError` for that packet. A stream whose first block is bad is not opened.
With `AudioDecoderOptions::verify` the decoder hashes the audio in the format of the source
file (big-endian/unsigned/reordered samples, DSD in 4096-byte blocks; the `qmode` of
`ID_CONFIG_BLOCK`/`ID_NEW_CONFIG_BLOCK`) and `finalize()` compares it to the stored MD5
(`ID_MD5_CHECKSUM` or `ID_ALT_MD5_CHECKSUM`). `verify_ok` is `None` when there is no MD5 or a
hybrid block was decoded without its correction block.

### DSD

WavPack DSD (raw, fast and high modes of `unpack_dsd.c`) is decoded to the DSD bit stream, not
to PCM: the sample format is `U8` with one byte (8 DSD samples, most significant bit first) per
channel per frame, and the `AudioSpec` rate is the DSD rate divided by 8. The track parameters
give the native DSD rate (e.g. 2.8224 MHz) with 1 bit per sample, and the timestamps, durations
and seeks count DSD bits. There is no DSD to PCM decimation.

### Other notes

* Sample rates that are not in the standard table are read from `ID_SAMPLE_RATE`; headers are
  found with the same plausibility scan as `read_next_header()`; the total length is found at
  the end of the file for streams that do not say it (and for streams that do not start at
  block index 0).
* Output is in the order of the file (the speaker mask), `ID_CHANNEL_IDENTITIES` is not used.
* The v1-v3 decoder is checked only with synthetic streams: the reference tools can no longer
  decode those versions.

IEEE float and hybrid-lossy int/float decoding were ported from the reference WavPack
library (BSD-3-Clause, see `LICENSE`/`NOTICE`); the ported functions are documented at
each call site with the corresponding upstream C source file.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
