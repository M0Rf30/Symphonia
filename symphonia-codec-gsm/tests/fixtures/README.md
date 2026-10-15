# Fixtures

Four 8 kHz mono sources, encoded with FFmpeg's `libgsm` (raw `fr_*.gsm`: 33-byte blocks) and
`libgsm_ms` (`ms_*.wav`: WAVE format 0x31, 65-byte blocks) encoders:

* `speech`: 4 s of speech (the TrueSpeech `a6.wav` sample from
  <https://samples.ffmpeg.org/A-codecs/truespeech/>, resampled to 8 kHz), 32000 samples.
* `loud`: 2 s of the same speech at +22 dB, clipped, 16000 samples.
* `noise`: 1 s of full-scale white noise, 8000 samples. It drives the synthesis filter into
  16-bit saturation, where FFmpeg wraps and the standard (and libgsm) saturates; the tests
  accept the FFmpeg difference for this fixture only and compare it with libgsm instead.
* `sweep`: 2 s logarithmic sine sweep, 16000 samples.

`fr_speech.{caf,mov}`, `ms_speech.{caf,mov,mkv}` are the `speech` blocks muxed by FFmpeg
(`-c:a libgsm` / `-c:a libgsm_ms`); AIFF-C `GSM ` is built by `tests/containers.rs` as FFmpeg cannot
write it.

The expected PCM is the output of `ffmpeg -i x -f s16le -` (FFmpeg 9.0.2 `gsm` / `gsm_ms`
decoders) and of `toast -d -l -c` (libgsm 1.0.22). `tests/ffmpeg_reference.rs` pins the FNV-1a
hash of it and compares live when `ffmpeg` / `toast` are on `PATH`.
