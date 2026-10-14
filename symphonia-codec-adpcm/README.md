# Symphonia ADPCM Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_adpcm-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-adpcm)

ADPCM audio decoders for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Support

| Encoding          | Bits per sample        | Channels         |
|-------------------|------------------------|------------------|
| Microsoft ADPCM   | 4                      | mono, stereo     |
| IMA ADPCM (WAV)   | 2, 3, 4, 5             | mono to 8        |
| IMA ADPCM (QT)    | 4                      | mono, stereo     |
| Yamaha ADPCM      | 4                      | mono, stereo     |
| SWF ADPCM (Flash) | 2, 3, 4, 5             | mono, stereo     |
| G.722             | 6, 7, 8 (per codeword) | mono             |
| G.726 (BE and LE) | 2, 3, 4, 5             | mono             |

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
