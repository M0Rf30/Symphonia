# Fixtures

Real TrueSpeech WAV files (WAVE format tag 0x22, 8 kHz mono, 32-byte blocks) from
<https://samples.ffmpeg.org/A-codecs/truespeech/>: `a6.wav`, `tada.wav`, `truespeech_a5.wav`.
The expected PCM is the output of `ffmpeg -i x.wav -f s16le -` (FFmpeg 9.0.2 `truespeech`
decoder); `tests/ffmpeg_reference.rs` pins its FNV-1a hash and compares live when `ffmpeg` exists.
