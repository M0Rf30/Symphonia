# symphonia-bundle-shorten

Pure Rust Shorten (`.shn`) lossless audio demuxer and decoder for Project Symphonia.

Supports Shorten versions 0 to 3 with unsigned 8-bit, signed 8-bit, and 16-bit samples of any byte
order, one to eight channels, and seeking (by reconstructing the stream up to the target). Ported
from FFmpeg's `libavcodec/shorten.c`; see `NOTICE` for details and attribution.
