# Symphonia LDAC Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_ldac-brightgreen?style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-ldac)

LDAC (Sony Bluetooth A2DP) decoder for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Supported

- Channel configurations: mono (1ch), dual channel (2ch) and stereo (2ch).
- Sample rates: 44.1 kHz, 48 kHz, 88.2 kHz and 96 kHz (128 or 256 samples per channel and frame).
- All encode quality modes (frame lengths from 22 up to 512 bytes per channel).

A packet holds one LDAC frame (the sync word is `0xaa`); any bytes after the first frame are
ignored, as in the SBC decoder. The sample rate, the channel configuration and the number of
samples are taken from the header of each frame, and the output is 16-bit planar `f32`
(1/32768 scale).

[`FrameHeader`] parses frame headers and gives the length of a frame, so a stream of consecutive
frames (for example the payload of a Bluetooth A2DP media packet) can be split.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more
details. The decoder is derived from the Apache-2.0 licensed AOSP `libldac` reference; see the
NOTICE file.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please
read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
