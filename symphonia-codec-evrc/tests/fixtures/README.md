# EVRC test fixtures

Truncated QCP files (RIFF `QLCM`) from the start of two EVRC recordings of the FFmpeg sample
collection, https://samples.ffmpeg.org/A-codecs/ (`01_ENTERSANDMAN_evrc_.qcp`,
`02_Lord_Of_The_Flies_evrc.qcp`); they are used here as decoder test vectors only.  The files are cut at a
packet boundary.

`entersandman_head.ref.s16` is the 16-bit little-endian output of
`ffmpeg -i entersandman_head.qcp -f s16le` (`evrc` decoder).
