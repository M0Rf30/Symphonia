# G.723.1 test streams

Raw G.723.1 frame streams, as written by `ffmpeg -f g723_1` (frame size 24/20/4/1 bytes selected by
the two LSBs of the first byte of each frame).

| File | Provenance |
|---|---|
| `real_6300.g723` | First 500 frames (6.3 kbit/s) of the payload of `g723_1.wav` from <https://samples.ffmpeg.org/A-codecs/>. |
| `real_5300.g723` | 5.3 kbit/s stream of `format-0x42-speechtest-MSG5.3.asf` from <https://samples.ffmpeg.org/A-codecs/>, extracted with `ffmpeg -c:a copy -f g723_1`. |
| `enc_6300.g723` | Encoded with the FFmpeg `g723_1` encoder (`-b:a 6300`; the encoder has no 5.3 kbit/s mode) from 7 s of the decoded `g723_1.wav`, amplified by 1.5. |
| `mixed.g723` | Frames of the three streams above, assembled by script, with hand-built SID (4 byte) and untransmitted (1 byte) frames, rate switches, and active frames corrupted to carry a forbidden pitch lag or gain code (frame erasures, including 5 in a row). |
| `random.g723`, `random2.g723` | Pseudo-random frames of every type (seeded; `random2.g723` is biased towards 0x00/0xff bytes to hit extreme parameters). |

The expected output is that of the FFmpeg 9.0.2 `g723_1` decoder (`ffmpeg -f g723_1 -i x
-f s16le -`); its FNV-1a hash is pinned in `tests/decode.rs`.
