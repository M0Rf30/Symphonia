# Symphonia ADPCM Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_adpcm-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-adpcm)

ADPCM audio decoders for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Support

| Encoding          | Bits per sample        | Channels         |
|-------------------|------------------------|------------------|
| Microsoft ADPCM   | 4                      | mono to 6        |
| IMA ADPCM (WAV)   | 2, 3, 4, 5             | mono to 8        |
| IMA ADPCM (QT)    | 4                      | mono, stereo     |
| IMA ADPCM (Duck DK3, DK4, Xbox, Westwood, Dialogic OKI) | 4 | mono, stereo (DK3: stereo) |
| Yamaha ADPCM      | 4                      | mono to 8        |
| Creative ADPCM    | 4                      | mono to 8        |
| SWF ADPCM (Flash) | 2, 3, 4, 5             | mono, stereo     |
| CRI ADX           | 4                      | mono to 6        |
| Sound Blaster Pro ADPCM | 2, 2.6, 4        | mono             |
| 8SVX Fibonacci and exponential delta | 4   | mono, stereo     |
| G.722             | 6, 7, 8 (per codeword) | mono to 8        |
| G.726 (BE and LE) | 2, 3, 4, 5             | mono to 8        |

FFmpeg decodes G.722, G.726, Yamaha, Creative and OKI ADPCM for at most 2 channels (G.722 and
G.726: 1). More channels are decoded as sample interleaved: the codes of the channels alternate,
and every channel decodes exactly like the mono stream of its own codes. Microsoft ADPCM with
more than 2 channels stores every channel as an independent mono block, as FFmpeg does.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
