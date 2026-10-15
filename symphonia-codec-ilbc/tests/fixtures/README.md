# Fixtures

Generated for this crate from `libilbc` (https://github.com/TimothyGu/libilbc, the WebRTC iLBC
codec, BSD-3-Clause) `sample.pcm` (a 3.2 s 8 kHz mono speech clip distributed with libilbc),
encoded with `WebRtcIlbcfix_Encode` and stored in the `.lbc` storage format of the FFmpeg `ilbc`
demuxer / RFC 3951 appendix (`#!iLBC20\n` / `#!iLBC30\n`, then back-to-back 38 / 50 byte frames).

| File | Content |
|---|---|
| `s20.lbc`, `s30.lbc` | clean 20 ms and 30 ms streams (163 and 108 frames) |
| `c20.lbc`, `c30.lbc` | the same with empty-frame flags set / the start sub-block field forced to 0 or invalid on a few frames, exercising concealment |
| `n20.lbc`, `n30.lbc` | the same with random bit errors (up to 3 flipped bits per frame) |

`s20.caf`, `s30.caf`, `s20.mov`, `s30.mov` are the clean streams remuxed (`-c copy`) by FFmpeg 9.0.2
into CAF and QuickTime containers.

Verified out of tree: the libilbc decoder with the enhancer disabled
(`WebRtcIlbcfix_InitDecode(.., 0)`) produces PCM identical to this decoder and to FFmpeg.
